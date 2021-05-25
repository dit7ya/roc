#![allow(clippy::too_many_arguments)]
use crate::llvm::bitcode::{
    build_dec_wrapper, build_eq_wrapper, build_inc_n_wrapper, build_inc_wrapper,
    build_transform_caller, call_bitcode_fn, call_void_bitcode_fn,
};
use crate::llvm::build::{
    allocate_with_refcount_help, cast_basic_basic, complex_bitcast, Env, InPlace, RocFunctionCall,
};
use crate::llvm::convert::{basic_type_from_layout, get_ptr_type};
use crate::llvm::refcounting::increment_refcount_layout;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::types::{BasicTypeEnum, PointerType};
use inkwell::values::{BasicValueEnum, FunctionValue, IntValue, PointerValue, StructValue};
use inkwell::{AddressSpace, IntPredicate};
use roc_builtins::bitcode;
use roc_mono::layout::{Builtin, Layout, LayoutIds, MemoryMode};

fn list_returned_from_zig<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    output: BasicValueEnum<'ctx>,
) -> BasicValueEnum<'ctx> {
    // per the C ABI, our list objects are passed between functions as an i128
    complex_bitcast(
        env.builder,
        output,
        super::convert::zig_list_type(env).into(),
        "from_i128",
    )
}

pub fn call_bitcode_fn_returns_list<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    args: &[BasicValueEnum<'ctx>],
    fn_name: &str,
) -> BasicValueEnum<'ctx> {
    let value = call_bitcode_fn(env, args, fn_name);

    list_returned_from_zig(env, value)
}

fn pass_element_as_opaque<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    element: BasicValueEnum<'ctx>,
) -> BasicValueEnum<'ctx> {
    let element_ptr = env.builder.build_alloca(element.get_type(), "element");
    env.builder.build_store(element_ptr, element);

    env.builder.build_bitcast(
        element_ptr,
        env.context.i8_type().ptr_type(AddressSpace::Generic),
        "to_opaque",
    )
}

fn pass_list_as_i128<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    list: BasicValueEnum<'ctx>,
) -> BasicValueEnum<'ctx> {
    complex_bitcast(env.builder, list, env.context.i128_type().into(), "to_i128")
}

pub fn layout_width<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    env.ptr_int()
        .const_int(layout.stack_size(env.ptr_bytes) as u64, false)
        .into()
}

pub fn pass_as_opaque<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    ptr: PointerValue<'ctx>,
) -> BasicValueEnum<'ctx> {
    env.builder.build_bitcast(
        ptr,
        env.context.i8_type().ptr_type(AddressSpace::Generic),
        "to_opaque",
    )
}

/// List.single : a -> List a
pub fn list_single<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    _inplace: InPlace,
    element: BasicValueEnum<'ctx>,
    element_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    call_bitcode_fn_returns_list(
        env,
        &[
            env.alignment_intvalue(element_layout),
            pass_element_as_opaque(env, element),
            layout_width(env, element_layout),
        ],
        &bitcode::LIST_SINGLE,
    )
}

/// List.repeat : Int, elem -> List elem
pub fn list_repeat<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    list_len: IntValue<'ctx>,
    element: BasicValueEnum<'ctx>,
    element_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let inc_element_fn = build_inc_n_wrapper(env, layout_ids, element_layout);

    call_bitcode_fn_returns_list(
        env,
        &[
            list_len.into(),
            env.alignment_intvalue(element_layout),
            pass_element_as_opaque(env, element),
            layout_width(env, element_layout),
            inc_element_fn.as_global_value().as_pointer_value().into(),
        ],
        bitcode::LIST_REPEAT,
    )
}

/// List.prepend : List elem, elem -> List elem
pub fn list_prepend<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    inplace: InPlace,
    original_wrapper: StructValue<'ctx>,
    elem: BasicValueEnum<'ctx>,
    elem_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;
    let ctx = env.context;

    // Load the usize length from the wrapper.
    let len = list_len(builder, original_wrapper);
    let elem_type = basic_type_from_layout(env, elem_layout);
    let ptr_type = get_ptr_type(&elem_type, AddressSpace::Generic);
    let list_ptr = load_list_ptr(builder, original_wrapper, ptr_type);

    // The output list length, which is the old list length + 1
    let new_list_len = env.builder.build_int_add(
        ctx.i64_type().const_int(1_u64, false),
        len,
        "new_list_length",
    );

    // Allocate space for the new array that we'll copy into.
    let clone_ptr = allocate_list(env, inplace, elem_layout, new_list_len);

    builder.build_store(clone_ptr, elem);

    let index_1_ptr = unsafe {
        builder.build_in_bounds_gep(
            clone_ptr,
            &[ctx.i64_type().const_int(1_u64, false)],
            "load_index",
        )
    };

    // Calculate the number of bytes we'll need to allocate.
    let elem_bytes = env
        .ptr_int()
        .const_int(elem_layout.stack_size(env.ptr_bytes) as u64, false);

    // This is the size of the list coming in, before we have added an element
    // to the beginning.
    let list_size = env
        .builder
        .build_int_mul(elem_bytes, len, "mul_old_len_by_elem_bytes");

    let ptr_bytes = env.ptr_bytes;

    if elem_layout.safe_to_memcpy() {
        // Copy the bytes from the original array into the new
        // one we just allocated
        //
        // TODO how do we decide when to do the small memcpy vs the normal one?
        builder
            .build_memcpy(index_1_ptr, ptr_bytes, list_ptr, ptr_bytes, list_size)
            .unwrap();
    } else {
        panic!("TODO Cranelift currently only knows how to clone list elements that are Copy.");
    }

    store_list(env, clone_ptr, new_list_len)
}

/// List.join : List (List elem) -> List elem
pub fn list_join<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    _inplace: InPlace,
    _parent: FunctionValue<'ctx>,
    outer_list: BasicValueEnum<'ctx>,
    outer_list_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    match outer_list_layout {
        Layout::Builtin(Builtin::EmptyList)
        | Layout::Builtin(Builtin::List(_, Layout::Builtin(Builtin::EmptyList))) => {
            // If the input list is empty, or if it is a list of empty lists
            // then simply return an empty list
            empty_list(env)
        }
        Layout::Builtin(Builtin::List(_, Layout::Builtin(Builtin::List(_, element_layout)))) => {
            call_bitcode_fn_returns_list(
                env,
                &[
                    pass_list_as_i128(env, outer_list),
                    env.alignment_intvalue(element_layout),
                    layout_width(env, element_layout),
                ],
                &bitcode::LIST_JOIN,
            )
        }
        _ => {
            unreachable!("Invalid List layout for List.join {:?}", outer_list_layout);
        }
    }
}

/// List.reverse : List elem -> List elem
pub fn list_reverse<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    _output_inplace: InPlace,
    list: BasicValueEnum<'ctx>,
    list_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let (_, element_layout) = match *list_layout {
        Layout::Builtin(Builtin::EmptyList) => (
            InPlace::InPlace,
            // this pointer will never actually be dereferenced
            Layout::Builtin(Builtin::Int64),
        ),
        Layout::Builtin(Builtin::List(memory_mode, elem_layout)) => (
            match memory_mode {
                MemoryMode::Unique => InPlace::InPlace,
                MemoryMode::Refcounted => InPlace::Clone,
            },
            *elem_layout,
        ),

        _ => unreachable!("Invalid layout {:?} in List.reverse", list_layout),
    };

    call_bitcode_fn_returns_list(
        env,
        &[
            pass_list_as_i128(env, list),
            env.alignment_intvalue(&element_layout),
            layout_width(env, &element_layout),
        ],
        &bitcode::LIST_REVERSE,
    )
}

pub fn list_get_unsafe<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    parent: FunctionValue<'ctx>,
    list_layout: &Layout<'a>,
    elem_index: IntValue<'ctx>,
    wrapper_struct: StructValue<'ctx>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    match list_layout {
        Layout::Builtin(Builtin::List(_, elem_layout)) => {
            let elem_type = basic_type_from_layout(env, elem_layout);
            let ptr_type = get_ptr_type(&elem_type, AddressSpace::Generic);
            // Load the pointer to the array data
            let array_data_ptr = load_list_ptr(builder, wrapper_struct, ptr_type);

            // Assume the bounds have already been checked earlier
            // (e.g. by List.get or List.first, which wrap List.#getUnsafe)
            let elem_ptr =
                unsafe { builder.build_in_bounds_gep(array_data_ptr, &[elem_index], "elem") };

            let result = builder.build_load(elem_ptr, "List.get");

            increment_refcount_layout(env, parent, layout_ids, 1, result, elem_layout);

            result
        }
        _ => {
            unreachable!(
                "Invalid List layout for ListGetUnsafe operation: {:?}",
                list_layout
            );
        }
    }
}

/// List.append : List elem, elem -> List elem
pub fn list_append<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    _inplace: InPlace,
    original_wrapper: StructValue<'ctx>,
    element: BasicValueEnum<'ctx>,
    element_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    call_bitcode_fn_returns_list(
        env,
        &[
            pass_list_as_i128(env, original_wrapper.into()),
            env.alignment_intvalue(&element_layout),
            pass_element_as_opaque(env, element),
            layout_width(env, element_layout),
        ],
        &bitcode::LIST_APPEND,
    )
}

/// List.drop : List elem, Nat -> List elem
pub fn list_drop<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    original_wrapper: StructValue<'ctx>,
    count: IntValue<'ctx>,
    element_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let dec_element_fn = build_dec_wrapper(env, layout_ids, &element_layout);
    call_bitcode_fn_returns_list(
        env,
        &[
            pass_list_as_i128(env, original_wrapper.into()),
            env.alignment_intvalue(&element_layout),
            layout_width(env, &element_layout),
            count.into(),
            dec_element_fn.as_global_value().as_pointer_value().into(),
        ],
        &bitcode::LIST_DROP,
    )
}

/// List.set : List elem, Nat, elem -> List elem
pub fn list_set<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    list: BasicValueEnum<'ctx>,
    index: IntValue<'ctx>,
    element: BasicValueEnum<'ctx>,
    element_layout: &'a Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let dec_element_fn = build_dec_wrapper(env, layout_ids, element_layout);

    let (length, bytes) = load_list(
        env.builder,
        list.into_struct_value(),
        env.context.i8_type().ptr_type(AddressSpace::Generic),
    );

    let new_bytes = call_bitcode_fn(
        env,
        &[
            // pass_list_as_i128(env, list),
            bytes.into(),
            length.into(),
            alignment_intvalue(env, &element_layout),
            index.into(),
            pass_element_as_opaque(env, element),
            layout_width(env, element_layout),
            dec_element_fn.as_global_value().as_pointer_value().into(),
        ],
        &bitcode::LIST_SET,
    );

    store_list(env, new_bytes.into_pointer_value(), length)
}

fn bounds_check_comparison<'ctx>(
    builder: &Builder<'ctx>,
    elem_index: IntValue<'ctx>,
    len: IntValue<'ctx>,
) -> IntValue<'ctx> {
    // Note: Check for index < length as the "true" condition,
    // to avoid misprediction. (In practice this should usually pass,
    // and CPUs generally default to predicting that a forward jump
    // shouldn't be taken; that is, they predict "else" won't be taken.)
    builder.build_int_compare(IntPredicate::ULT, elem_index, len, "bounds_check")
}

/// List.len : List elem -> Int
pub fn list_len<'ctx>(
    builder: &Builder<'ctx>,
    wrapper_struct: StructValue<'ctx>,
) -> IntValue<'ctx> {
    builder
        .build_extract_value(wrapper_struct, Builtin::WRAPPER_LEN, "list_len")
        .unwrap()
        .into_int_value()
}

pub enum ListWalk {
    Walk,
    WalkBackwards,
    WalkUntil,
    WalkBackwardsUntil,
}

pub fn list_walk_generic<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    roc_function_call: RocFunctionCall<'ctx>,
    list: BasicValueEnum<'ctx>,
    element_layout: &Layout<'a>,
    default: BasicValueEnum<'ctx>,
    default_layout: &Layout<'a>,
    variant: ListWalk,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let zig_function = match variant {
        ListWalk::Walk => bitcode::LIST_WALK,
        ListWalk::WalkBackwards => bitcode::LIST_WALK_BACKWARDS,
        ListWalk::WalkUntil => bitcode::LIST_WALK_UNTIL,
        ListWalk::WalkBackwardsUntil => todo!(),
    };

    let default_ptr = builder.build_alloca(default.get_type(), "default_ptr");
    env.builder.build_store(default_ptr, default);

    let result_ptr = env.builder.build_alloca(default.get_type(), "result");

    match variant {
        ListWalk::Walk | ListWalk::WalkBackwards => {
            call_void_bitcode_fn(
                env,
                &[
                    pass_list_as_i128(env, list),
                    roc_function_call.caller.into(),
                    pass_as_opaque(env, roc_function_call.data),
                    roc_function_call.inc_n_data.into(),
                    roc_function_call.data_is_owned.into(),
                    pass_as_opaque(env, default_ptr),
                    env.alignment_intvalue(&element_layout),
                    layout_width(env, element_layout),
                    layout_width(env, default_layout),
                    pass_as_opaque(env, result_ptr),
                ],
                zig_function,
            );
        }
        ListWalk::WalkUntil | ListWalk::WalkBackwardsUntil => {
            let dec_element_fn = build_dec_wrapper(env, layout_ids, element_layout);
            call_void_bitcode_fn(
                env,
                &[
                    pass_list_as_i128(env, list),
                    roc_function_call.caller.into(),
                    pass_as_opaque(env, roc_function_call.data),
                    roc_function_call.inc_n_data.into(),
                    roc_function_call.data_is_owned.into(),
                    pass_as_opaque(env, default_ptr),
                    env.alignment_intvalue(&element_layout),
                    layout_width(env, element_layout),
                    layout_width(env, default_layout),
                    dec_element_fn.as_global_value().as_pointer_value().into(),
                    pass_as_opaque(env, result_ptr),
                ],
                zig_function,
            );
        }
    }

    env.builder.build_load(result_ptr, "load_result")
}

#[allow(dead_code)]
#[repr(u8)]
enum IntWidth {
    U8,
    U16,
    U32,
    U64,
    U128,
    I8,
    I16,
    I32,
    I64,
    I128,
    Usize,
}

impl From<roc_mono::layout::Builtin<'_>> for IntWidth {
    fn from(builtin: Builtin) -> Self {
        use IntWidth::*;

        match builtin {
            Builtin::Int128 => I128,
            Builtin::Int64 => I64,
            Builtin::Int32 => I32,
            Builtin::Int16 => I16,
            Builtin::Int8 => I8,
            Builtin::Usize => Usize,
            _ => unreachable!(),
        }
    }
}

/// List.range : Int a, Int a -> List (Int a)
pub fn list_range<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    builtin: Builtin<'a>,
    low: IntValue<'ctx>,
    high: IntValue<'ctx>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let low_ptr = builder.build_alloca(low.get_type(), "low_ptr");
    env.builder.build_store(low_ptr, low);

    let high_ptr = builder.build_alloca(high.get_type(), "high_ptr");
    env.builder.build_store(high_ptr, high);

    let int_width = env
        .context
        .i8_type()
        .const_int(IntWidth::from(builtin) as u64, false)
        .into();

    call_bitcode_fn(
        env,
        &[
            int_width,
            pass_as_opaque(env, low_ptr),
            pass_as_opaque(env, high_ptr),
        ],
        &bitcode::LIST_RANGE,
    )
}

/// List.contains : List elem, elem -> Bool
pub fn list_contains<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    element: BasicValueEnum<'ctx>,
    element_layout: &Layout<'a>,
    list: BasicValueEnum<'ctx>,
) -> BasicValueEnum<'ctx> {
    let eq_fn = build_eq_wrapper(env, layout_ids, element_layout)
        .as_global_value()
        .as_pointer_value()
        .into();

    call_bitcode_fn(
        env,
        &[
            pass_list_as_i128(env, list),
            pass_element_as_opaque(env, element),
            layout_width(env, element_layout),
            eq_fn,
        ],
        bitcode::LIST_CONTAINS,
    )
}

/// List.keepIf : List elem, (elem -> Bool) -> List elem
pub fn list_keep_if<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    roc_function_call: RocFunctionCall<'ctx>,
    list: BasicValueEnum<'ctx>,
    element_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let inc_element_fn = build_inc_wrapper(env, layout_ids, element_layout);
    let dec_element_fn = build_dec_wrapper(env, layout_ids, element_layout);

    call_bitcode_fn_returns_list(
        env,
        &[
            pass_list_as_i128(env, list),
            roc_function_call.caller.into(),
            pass_as_opaque(env, roc_function_call.data),
            roc_function_call.inc_n_data.into(),
            roc_function_call.data_is_owned.into(),
            env.alignment_intvalue(&element_layout),
            layout_width(env, element_layout),
            inc_element_fn.as_global_value().as_pointer_value().into(),
            dec_element_fn.as_global_value().as_pointer_value().into(),
        ],
        &bitcode::LIST_KEEP_IF,
    )
}

/// List.keepOks : List before, (before -> Result after *) -> List after
pub fn list_keep_oks<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    roc_function_call: RocFunctionCall<'ctx>,
    function_layout: &Layout<'a>,
    list: BasicValueEnum<'ctx>,
    before_layout: &Layout<'a>,
    after_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    // Layout of the `Result after *`
    let result_layout = match function_layout {
        Layout::FunctionPointer(_, ret) => ret,
        Layout::Closure(_, _, ret) => ret,
        _ => unreachable!("not a callable layout"),
    };

    let dec_result_fn = build_dec_wrapper(env, layout_ids, result_layout);

    call_bitcode_fn(
        env,
        &[
            pass_list_as_i128(env, list),
            roc_function_call.caller.into(),
            pass_as_opaque(env, roc_function_call.data),
            roc_function_call.inc_n_data.into(),
            roc_function_call.data_is_owned.into(),
            env.alignment_intvalue(&before_layout),
            layout_width(env, before_layout),
            layout_width(env, result_layout),
            layout_width(env, after_layout),
            dec_result_fn.as_global_value().as_pointer_value().into(),
        ],
        bitcode::LIST_KEEP_OKS,
    )
}

/// List.keepErrs : List before, (before -> Result * after) -> List after
pub fn list_keep_errs<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    roc_function_call: RocFunctionCall<'ctx>,
    function_layout: &Layout<'a>,
    list: BasicValueEnum<'ctx>,
    before_layout: &Layout<'a>,
    after_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    // Layout of the `Result after *`
    let result_layout = match function_layout {
        Layout::FunctionPointer(_, ret) => ret,
        Layout::Closure(_, _, ret) => ret,
        _ => unreachable!("not a callable layout"),
    };

    let dec_result_fn = build_dec_wrapper(env, layout_ids, result_layout);

    call_bitcode_fn(
        env,
        &[
            pass_list_as_i128(env, list),
            roc_function_call.caller.into(),
            pass_as_opaque(env, roc_function_call.data),
            roc_function_call.inc_n_data.into(),
            roc_function_call.data_is_owned.into(),
            env.alignment_intvalue(&before_layout),
            layout_width(env, before_layout),
            layout_width(env, result_layout),
            layout_width(env, after_layout),
            dec_result_fn.as_global_value().as_pointer_value().into(),
        ],
        bitcode::LIST_KEEP_ERRS,
    )
}

pub fn list_keep_result<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    transform: FunctionValue<'ctx>,
    transform_layout: Layout<'a>,
    closure_data: BasicValueEnum<'ctx>,
    closure_data_layout: Layout<'a>,
    list: BasicValueEnum<'ctx>,
    before_layout: &Layout<'a>,
    after_layout: &Layout<'a>,
    op: &str,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let result_layout = match transform_layout {
        Layout::FunctionPointer(_, ret) => ret,
        Layout::Closure(_, _, ret) => ret,
        _ => unreachable!("not a callable layout"),
    };

    let closure_data_ptr = builder.build_alloca(closure_data.get_type(), "closure_data_ptr");
    env.builder.build_store(closure_data_ptr, closure_data);

    let stepper_caller =
        build_transform_caller(env, transform, closure_data_layout, &[*before_layout])
            .as_global_value()
            .as_pointer_value();

    let inc_closure = build_inc_wrapper(env, layout_ids, &transform_layout);
    let dec_result_fn = build_dec_wrapper(env, layout_ids, result_layout);

    call_bitcode_fn(
        env,
        &[
            pass_list_as_i128(env, list),
            pass_as_opaque(env, closure_data_ptr),
            stepper_caller.into(),
            env.alignment_intvalue(&before_layout),
            layout_width(env, before_layout),
            layout_width(env, after_layout),
            layout_width(env, result_layout),
            inc_closure.as_global_value().as_pointer_value().into(),
            dec_result_fn.as_global_value().as_pointer_value().into(),
        ],
        op,
    )
}

/// List.sortWith : List a, (a, a -> Ordering) -> List a
pub fn list_sort_with<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    roc_function_call: RocFunctionCall<'ctx>,
    compare_wrapper: PointerValue<'ctx>,
    list: BasicValueEnum<'ctx>,
    element_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    call_bitcode_fn_returns_list(
        env,
        &[
            pass_list_as_i128(env, list),
            compare_wrapper.into(),
            pass_as_opaque(env, roc_function_call.data),
            roc_function_call.inc_n_data.into(),
            roc_function_call.data_is_owned.into(),
            env.alignment_intvalue(&element_layout),
            layout_width(env, element_layout),
        ],
        bitcode::LIST_SORT_WITH,
    )
}

/// List.mapWithIndex : List before, (Nat, before -> after) -> List after
pub fn list_map_with_index<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    roc_function_call: RocFunctionCall<'ctx>,
    list: BasicValueEnum<'ctx>,
    element_layout: &Layout<'a>,
    return_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    call_bitcode_fn_returns_list(
        env,
        &[
            pass_list_as_i128(env, list),
            roc_function_call.caller.into(),
            pass_as_opaque(env, roc_function_call.data),
            roc_function_call.inc_n_data.into(),
            roc_function_call.data_is_owned.into(),
            env.alignment_intvalue(&element_layout),
            layout_width(env, element_layout),
            layout_width(env, return_layout),
        ],
        bitcode::LIST_MAP_WITH_INDEX,
    )
}

/// List.map : List before, (before -> after) -> List after
pub fn list_map<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    roc_function_call: RocFunctionCall<'ctx>,
    list: BasicValueEnum<'ctx>,
    element_layout: &Layout<'a>,
    return_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    call_bitcode_fn_returns_list(
        env,
        &[
            pass_list_as_i128(env, list),
            roc_function_call.caller.into(),
            pass_as_opaque(env, roc_function_call.data),
            roc_function_call.inc_n_data.into(),
            roc_function_call.data_is_owned.into(),
            env.alignment_intvalue(&element_layout),
            layout_width(env, element_layout),
            layout_width(env, return_layout),
        ],
        bitcode::LIST_MAP,
    )
}

pub fn list_map2<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    roc_function_call: RocFunctionCall<'ctx>,
    list1: BasicValueEnum<'ctx>,
    list2: BasicValueEnum<'ctx>,
    element1_layout: &Layout<'a>,
    element2_layout: &Layout<'a>,
    return_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let dec_a = build_dec_wrapper(env, layout_ids, element1_layout);
    let dec_b = build_dec_wrapper(env, layout_ids, element2_layout);

    call_bitcode_fn(
        env,
        &[
            pass_list_as_i128(env, list1),
            pass_list_as_i128(env, list2),
            roc_function_call.caller.into(),
            pass_as_opaque(env, roc_function_call.data),
            roc_function_call.inc_n_data.into(),
            roc_function_call.data_is_owned.into(),
            env.alignment_intvalue(return_layout),
            layout_width(env, element1_layout),
            layout_width(env, element2_layout),
            layout_width(env, return_layout),
            dec_a.as_global_value().as_pointer_value().into(),
            dec_b.as_global_value().as_pointer_value().into(),
        ],
        bitcode::LIST_MAP2,
    )
}

pub fn list_map3<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    layout_ids: &mut LayoutIds<'a>,
    roc_function_call: RocFunctionCall<'ctx>,
    list1: BasicValueEnum<'ctx>,
    list2: BasicValueEnum<'ctx>,
    list3: BasicValueEnum<'ctx>,
    element1_layout: &Layout<'a>,
    element2_layout: &Layout<'a>,
    element3_layout: &Layout<'a>,
    result_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    let dec_a = build_dec_wrapper(env, layout_ids, element1_layout);
    let dec_b = build_dec_wrapper(env, layout_ids, element2_layout);
    let dec_c = build_dec_wrapper(env, layout_ids, element3_layout);

    call_bitcode_fn_returns_list(
        env,
        &[
            pass_list_as_i128(env, list1),
            pass_list_as_i128(env, list2),
            pass_list_as_i128(env, list3),
            roc_function_call.caller.into(),
            pass_as_opaque(env, roc_function_call.data),
            roc_function_call.inc_n_data.into(),
            roc_function_call.data_is_owned.into(),
            env.alignment_intvalue(result_layout),
            layout_width(env, element1_layout),
            layout_width(env, element2_layout),
            layout_width(env, element3_layout),
            layout_width(env, result_layout),
            dec_a.as_global_value().as_pointer_value().into(),
            dec_b.as_global_value().as_pointer_value().into(),
            dec_c.as_global_value().as_pointer_value().into(),
        ],
        bitcode::LIST_MAP3,
    )
}

/// List.concat : List elem, List elem -> List elem
pub fn list_concat<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    _inplace: InPlace,
    _parent: FunctionValue<'ctx>,
    first_list: BasicValueEnum<'ctx>,
    second_list: BasicValueEnum<'ctx>,
    list_layout: &Layout<'a>,
) -> BasicValueEnum<'ctx> {
    match list_layout {
        Layout::Builtin(Builtin::EmptyList) => {
            // If the input list is empty, or if it is a list of empty lists
            // then simply return an empty list
            empty_list(env)
        }
        Layout::Builtin(Builtin::List(_, elem_layout)) => call_bitcode_fn_returns_list(
            env,
            &[
                pass_list_as_i128(env, first_list),
                pass_list_as_i128(env, second_list),
                env.alignment_intvalue(elem_layout),
                layout_width(env, elem_layout),
            ],
            &bitcode::LIST_CONCAT,
        ),
        _ => {
            unreachable!("Invalid List layout for List.concat {:?}", list_layout);
        }
    }
}

pub fn decrementing_elem_loop<'ctx, LoopFn>(
    builder: &Builder<'ctx>,
    ctx: &'ctx Context,
    parent: FunctionValue<'ctx>,
    ptr: PointerValue<'ctx>,
    len: IntValue<'ctx>,
    index_name: &str,
    mut loop_fn: LoopFn,
) -> PointerValue<'ctx>
where
    LoopFn: FnMut(IntValue<'ctx>, BasicValueEnum<'ctx>),
{
    decrementing_index_loop(builder, ctx, parent, len, index_name, |index| {
        // The pointer to the element in the list
        let elem_ptr = unsafe { builder.build_in_bounds_gep(ptr, &[index], "load_index") };

        let elem = builder.build_load(elem_ptr, "get_elem");

        loop_fn(index, elem);
    })
}

// a for-loop from the back to the front
fn decrementing_index_loop<'ctx, LoopFn>(
    builder: &Builder<'ctx>,
    ctx: &'ctx Context,
    parent: FunctionValue<'ctx>,
    end: IntValue<'ctx>,
    index_name: &str,
    mut loop_fn: LoopFn,
) -> PointerValue<'ctx>
where
    LoopFn: FnMut(IntValue<'ctx>),
{
    // constant 1i64
    let one = ctx.i64_type().const_int(1, false);

    // allocate a stack slot for the current index
    let index_alloca = builder.build_alloca(ctx.i64_type(), index_name);

    // we assume `end` is the length of the list
    // the final index is therefore `end - 1`
    let end_index = builder.build_int_sub(end, one, "end_index");
    builder.build_store(index_alloca, end_index);

    let loop_bb = ctx.append_basic_block(parent, "loop");
    builder.build_unconditional_branch(loop_bb);
    builder.position_at_end(loop_bb);

    let current_index = builder
        .build_load(index_alloca, index_name)
        .into_int_value();

    let next_index = builder.build_int_sub(current_index, one, "nextindex");

    builder.build_store(index_alloca, next_index);

    // The body of the loop
    loop_fn(current_index);

    // #index >= 0
    let condition = builder.build_int_compare(
        IntPredicate::SGE,
        next_index,
        ctx.i64_type().const_zero(),
        "bounds_check",
    );

    let after_loop_bb = ctx.append_basic_block(parent, "after_outer_loop_1");

    builder.build_conditional_branch(condition, loop_bb, after_loop_bb);
    builder.position_at_end(after_loop_bb);

    index_alloca
}

pub fn incrementing_elem_loop<'ctx, LoopFn>(
    builder: &Builder<'ctx>,
    ctx: &'ctx Context,
    parent: FunctionValue<'ctx>,
    ptr: PointerValue<'ctx>,
    len: IntValue<'ctx>,
    index_name: &str,
    mut loop_fn: LoopFn,
) -> PointerValue<'ctx>
where
    LoopFn: FnMut(IntValue<'ctx>, BasicValueEnum<'ctx>),
{
    incrementing_index_loop(builder, ctx, parent, len, index_name, |index| {
        // The pointer to the element in the list
        let elem_ptr = unsafe { builder.build_in_bounds_gep(ptr, &[index], "load_index") };

        let elem = builder.build_load(elem_ptr, "get_elem");

        loop_fn(index, elem);
    })
}

// This helper simulates a basic for loop, where
// and index increments up from 0 to some end value
pub fn incrementing_index_loop<'ctx, LoopFn>(
    builder: &Builder<'ctx>,
    ctx: &'ctx Context,
    parent: FunctionValue<'ctx>,
    end: IntValue<'ctx>,
    index_name: &str,
    mut loop_fn: LoopFn,
) -> PointerValue<'ctx>
where
    LoopFn: FnMut(IntValue<'ctx>),
{
    // constant 1i64
    let one = ctx.i64_type().const_int(1, false);

    // allocate a stack slot for the current index
    let index_alloca = builder.build_alloca(ctx.i64_type(), index_name);
    builder.build_store(index_alloca, ctx.i64_type().const_zero());

    let loop_bb = ctx.append_basic_block(parent, "loop");
    builder.build_unconditional_branch(loop_bb);
    builder.position_at_end(loop_bb);

    let curr_index = builder
        .build_load(index_alloca, index_name)
        .into_int_value();
    let next_index = builder.build_int_add(curr_index, one, "nextindex");

    builder.build_store(index_alloca, next_index);

    // The body of the loop
    loop_fn(curr_index);

    // #index < end
    let loop_end_cond = bounds_check_comparison(builder, next_index, end);

    let after_loop_bb = ctx.append_basic_block(parent, "after_outer_loop_2");

    builder.build_conditional_branch(loop_end_cond, loop_bb, after_loop_bb);
    builder.position_at_end(after_loop_bb);

    index_alloca
}

pub fn build_basic_phi2<'a, 'ctx, 'env, PassFn, FailFn>(
    env: &Env<'a, 'ctx, 'env>,
    parent: FunctionValue<'ctx>,
    comparison: IntValue<'ctx>,
    mut build_pass: PassFn,
    mut build_fail: FailFn,
    ret_type: BasicTypeEnum<'ctx>,
) -> BasicValueEnum<'ctx>
where
    PassFn: FnMut() -> BasicValueEnum<'ctx>,
    FailFn: FnMut() -> BasicValueEnum<'ctx>,
{
    let builder = env.builder;
    let context = env.context;

    // build blocks
    let then_block = context.append_basic_block(parent, "then");
    let else_block = context.append_basic_block(parent, "else");
    let cont_block = context.append_basic_block(parent, "branchcont");

    builder.build_conditional_branch(comparison, then_block, else_block);

    // build then block
    builder.position_at_end(then_block);
    let then_val = build_pass();
    builder.build_unconditional_branch(cont_block);

    let then_block = builder.get_insert_block().unwrap();

    // build else block
    builder.position_at_end(else_block);
    let else_val = build_fail();
    builder.build_unconditional_branch(cont_block);

    let else_block = builder.get_insert_block().unwrap();

    // emit merge block
    builder.position_at_end(cont_block);

    let phi = builder.build_phi(ret_type, "branch");

    phi.add_incoming(&[(&then_val, then_block), (&else_val, else_block)]);

    phi.as_basic_value()
}

pub fn empty_polymorphic_list<'a, 'ctx, 'env>(env: &Env<'a, 'ctx, 'env>) -> BasicValueEnum<'ctx> {
    let struct_type = super::convert::zig_list_type(env);

    // The pointer should be null (aka zero) and the length should be zero,
    // so the whole struct should be a const_zero
    BasicValueEnum::StructValue(struct_type.const_zero())
}

// TODO investigate: does this cause problems when the layout is known? this value is now not refcounted!
pub fn empty_list<'a, 'ctx, 'env>(env: &Env<'a, 'ctx, 'env>) -> BasicValueEnum<'ctx> {
    let struct_type = super::convert::zig_list_type(env);

    // The pointer should be null (aka zero) and the length should be zero,
    // so the whole struct should be a const_zero
    BasicValueEnum::StructValue(struct_type.const_zero())
}

pub fn load_list<'ctx>(
    builder: &Builder<'ctx>,
    wrapper_struct: StructValue<'ctx>,
    ptr_type: PointerType<'ctx>,
) -> (IntValue<'ctx>, PointerValue<'ctx>) {
    let ptr = load_list_ptr(builder, wrapper_struct, ptr_type);

    let length = builder
        .build_extract_value(wrapper_struct, Builtin::WRAPPER_LEN, "list_len")
        .unwrap()
        .into_int_value();

    (length, ptr)
}

pub fn load_list_ptr<'ctx>(
    builder: &Builder<'ctx>,
    wrapper_struct: StructValue<'ctx>,
    ptr_type: PointerType<'ctx>,
) -> PointerValue<'ctx> {
    // a `*mut u8` pointer
    let generic_ptr = builder
        .build_extract_value(wrapper_struct, Builtin::WRAPPER_PTR, "read_list_ptr")
        .unwrap()
        .into_pointer_value();

    // cast to the expected pointer type
    cast_basic_basic(builder, generic_ptr.into(), ptr_type.into()).into_pointer_value()
}

pub fn allocate_list<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    inplace: InPlace,
    elem_layout: &Layout<'a>,
    number_of_elements: IntValue<'ctx>,
) -> PointerValue<'ctx> {
    let builder = env.builder;
    let ctx = env.context;

    let len_type = env.ptr_int();
    let elem_bytes = elem_layout.stack_size(env.ptr_bytes) as u64;
    let bytes_per_element = len_type.const_int(elem_bytes, false);
    let number_of_data_bytes =
        builder.build_int_mul(bytes_per_element, number_of_elements, "data_length");

    let rc1 = match inplace {
        InPlace::InPlace => number_of_elements,
        InPlace::Clone => {
            // the refcount of a new list is initially 1
            // we assume that the list is indeed used (dead variables are eliminated)
            crate::llvm::refcounting::refcount_1(ctx, env.ptr_bytes)
        }
    };

    allocate_with_refcount_help(env, elem_layout, number_of_data_bytes, rc1)
}

pub fn store_list<'a, 'ctx, 'env>(
    env: &Env<'a, 'ctx, 'env>,
    pointer_to_first_element: PointerValue<'ctx>,
    len: IntValue<'ctx>,
) -> BasicValueEnum<'ctx> {
    let builder = env.builder;

    let struct_type = super::convert::zig_list_type(env);

    let mut struct_val;

    // Store the pointer
    struct_val = builder
        .build_insert_value(
            struct_type.get_undef(),
            pass_as_opaque(env, pointer_to_first_element),
            Builtin::WRAPPER_PTR,
            "insert_ptr_store_list",
        )
        .unwrap();

    // Store the length
    struct_val = builder
        .build_insert_value(struct_val, len, Builtin::WRAPPER_LEN, "insert_len")
        .unwrap();

    builder.build_bitcast(
        struct_val.into_struct_value(),
        super::convert::zig_list_type(env),
        "cast_collection",
    )
}
