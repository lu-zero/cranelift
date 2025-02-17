//! Stand-alone WebAssembly to Cranelift IR translator.
//!
//! This module defines the `FuncTranslator` type which can translate a single WebAssembly
//! function to Cranelift IR guided by a `FuncEnvironment` which provides information about the
//! WebAssembly module and the runtime environment.

use crate::code_translator::translate_operator;
use crate::environ::{FuncEnvironment, ReturnMode, WasmError, WasmResult};
use crate::state::TranslationState;
use crate::translation_utils::get_vmctx_value_label;
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::{self, Ebb, InstBuilder, ValueLabel};
use cranelift_codegen::timing;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use log::info;
use wasmparser::{self, BinaryReader};

/// WebAssembly to Cranelift IR function translator.
///
/// A `FuncTranslator` is used to translate a binary WebAssembly function into Cranelift IR guided
/// by a `FuncEnvironment` object. A single translator instance can be reused to translate multiple
/// functions which will reduce heap allocation traffic.
pub struct FuncTranslator {
    func_ctx: FunctionBuilderContext,
    state: TranslationState,
}

impl FuncTranslator {
    /// Create a new translator.
    pub fn new() -> Self {
        Self {
            func_ctx: FunctionBuilderContext::new(),
            state: TranslationState::new(),
        }
    }

    /// Translate a binary WebAssembly function.
    ///
    /// The `code` slice contains the binary WebAssembly *function code* as it appears in the code
    /// section of a WebAssembly module, not including the initial size of the function code. The
    /// slice is expected to contain two parts:
    ///
    /// - The declaration of *locals*, and
    /// - The function *body* as an expression.
    ///
    /// See [the WebAssembly specification][wasm].
    ///
    /// [wasm]: https://webassembly.github.io/spec/core/binary/modules.html#code-section
    ///
    /// The Cranelift IR function `func` should be completely empty except for the `func.signature`
    /// and `func.name` fields. The signature may contain special-purpose arguments which are not
    /// regarded as WebAssembly local variables. Any signature arguments marked as
    /// `ArgumentPurpose::Normal` are made accessible as WebAssembly local variables.
    ///
    pub fn translate<FE: FuncEnvironment + ?Sized>(
        &mut self,
        code: &[u8],
        code_offset: usize,
        func: &mut ir::Function,
        environ: &mut FE,
    ) -> WasmResult<()> {
        self.translate_from_reader(
            BinaryReader::new_with_offset(code, code_offset),
            func,
            environ,
        )
    }

    /// Translate a binary WebAssembly function from a `BinaryReader`.
    pub fn translate_from_reader<FE: FuncEnvironment + ?Sized>(
        &mut self,
        mut reader: BinaryReader,
        func: &mut ir::Function,
        environ: &mut FE,
    ) -> WasmResult<()> {
        let _tt = timing::wasm_translate_function();
        info!(
            "translate({} bytes, {}{})",
            reader.bytes_remaining(),
            func.name,
            func.signature
        );
        debug_assert_eq!(func.dfg.num_ebbs(), 0, "Function must be empty");
        debug_assert_eq!(func.dfg.num_insts(), 0, "Function must be empty");

        // This clears the `FunctionBuilderContext`.
        let mut builder = FunctionBuilder::new(func, &mut self.func_ctx);
        builder.set_srcloc(cur_srcloc(&reader));
        let entry_block = builder.create_ebb();
        builder.append_ebb_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block); // This also creates values for the arguments.
        builder.seal_block(entry_block); // Declare all predecessors known.

        // Make sure the entry block is inserted in the layout before we make any callbacks to
        // `environ`. The callback functions may need to insert things in the entry block.
        builder.ensure_inserted_ebb();

        let num_params = declare_wasm_parameters(&mut builder, entry_block);

        // Set up the translation state with a single pushed control block representing the whole
        // function and its return values.
        let exit_block = builder.create_ebb();
        builder.append_ebb_params_for_function_returns(exit_block);
        self.state.initialize(&builder.func.signature, exit_block);

        parse_local_decls(&mut reader, &mut builder, num_params)?;
        parse_function_body(reader, &mut builder, &mut self.state, environ)?;

        builder.finalize();
        Ok(())
    }
}

/// Declare local variables for the signature parameters that correspond to WebAssembly locals.
///
/// Return the number of local variables declared.
fn declare_wasm_parameters(builder: &mut FunctionBuilder, entry_block: Ebb) -> usize {
    let sig_len = builder.func.signature.params.len();
    let mut next_local = 0;
    for i in 0..sig_len {
        let param_type = builder.func.signature.params[i];
        // There may be additional special-purpose parameters following the normal WebAssembly
        // signature parameters. For example, a `vmctx` pointer.
        if param_type.purpose == ir::ArgumentPurpose::Normal {
            // This is a normal WebAssembly signature parameter, so create a local for it.
            let local = Variable::new(next_local);
            builder.declare_var(local, param_type.value_type);
            next_local += 1;

            let param_value = builder.ebb_params(entry_block)[i];
            builder.def_var(local, param_value);
        }
        if param_type.purpose == ir::ArgumentPurpose::VMContext {
            let param_value = builder.ebb_params(entry_block)[i];
            builder.set_val_label(param_value, get_vmctx_value_label());
        }
    }

    next_local
}

/// Parse the local variable declarations that precede the function body.
///
/// Declare local variables, starting from `num_params`.
fn parse_local_decls(
    reader: &mut BinaryReader,
    builder: &mut FunctionBuilder,
    num_params: usize,
) -> WasmResult<()> {
    let mut next_local = num_params;
    let local_count = reader.read_local_count()?;

    let mut locals_total = 0;
    for _ in 0..local_count {
        builder.set_srcloc(cur_srcloc(reader));
        let (count, ty) = reader.read_local_decl(&mut locals_total)?;
        declare_locals(builder, count, ty, &mut next_local)?;
    }

    Ok(())
}

/// Declare `count` local variables of the same type, starting from `next_local`.
///
/// Fail of too many locals are declared in the function, or if the type is not valid for a local.
fn declare_locals(
    builder: &mut FunctionBuilder,
    count: u32,
    wasm_type: wasmparser::Type,
    next_local: &mut usize,
) -> WasmResult<()> {
    // All locals are initialized to 0.
    use wasmparser::Type::*;
    let zeroval = match wasm_type {
        I32 => builder.ins().iconst(ir::types::I32, 0),
        I64 => builder.ins().iconst(ir::types::I64, 0),
        F32 => builder.ins().f32const(ir::immediates::Ieee32::with_bits(0)),
        F64 => builder.ins().f64const(ir::immediates::Ieee64::with_bits(0)),
        _ => return Err(WasmError::Unsupported("unsupported local type")),
    };

    let ty = builder.func.dfg.value_type(zeroval);
    for _ in 0..count {
        let local = Variable::new(*next_local);
        builder.declare_var(local, ty);
        builder.def_var(local, zeroval);
        builder.set_val_label(zeroval, ValueLabel::new(*next_local));
        *next_local += 1;
    }
    Ok(())
}

/// Parse the function body in `reader`.
///
/// This assumes that the local variable declarations have already been parsed and function
/// arguments and locals are declared in the builder.
fn parse_function_body<FE: FuncEnvironment + ?Sized>(
    mut reader: BinaryReader,
    builder: &mut FunctionBuilder,
    state: &mut TranslationState,
    environ: &mut FE,
) -> WasmResult<()> {
    // The control stack is initialized with a single block representing the whole function.
    debug_assert_eq!(state.control_stack.len(), 1, "State not initialized");

    // Keep going until the final `End` operator which pops the outermost block.
    while !state.control_stack.is_empty() {
        builder.set_srcloc(cur_srcloc(&reader));
        let op = reader.read_operator()?;
        environ.before_translate_operator(&op, builder, state)?;
        translate_operator(&op, builder, state, environ)?;
        environ.after_translate_operator(&op, builder, state)?;
    }

    // The final `End` operator left us in the exit block where we need to manually add a return
    // instruction.
    //
    // If the exit block is unreachable, it may not have the correct arguments, so we would
    // generate a return instruction that doesn't match the signature.
    if state.reachable {
        debug_assert!(builder.is_pristine());
        if !builder.is_unreachable() {
            match environ.return_mode() {
                ReturnMode::NormalReturns => builder.ins().return_(&state.stack),
                ReturnMode::FallthroughReturn => builder.ins().fallthrough_return(&state.stack),
            };
        }
    }

    // Discard any remaining values on the stack. Either we just returned them,
    // or the end of the function is unreachable.
    state.stack.clear();

    debug_assert!(reader.eof());

    Ok(())
}

/// Get the current source location from a reader.
fn cur_srcloc(reader: &BinaryReader) -> ir::SourceLoc {
    // We record source locations as byte code offsets relative to the beginning of the file.
    // This will wrap around if byte code is larger than 4 GB.
    ir::SourceLoc::new(reader.original_position() as u32)
}

#[cfg(test)]
mod tests {
    use super::{FuncTranslator, ReturnMode};
    use crate::environ::DummyEnvironment;
    use cranelift_codegen::ir::types::I32;
    use cranelift_codegen::{ir, isa, settings, Context};
    use log::debug;
    use target_lexicon::PointerWidth;

    #[test]
    fn small1() {
        // Implicit return.
        //
        // (func $small1 (param i32) (result i32)
        //     (i32.add (get_local 0) (i32.const 1))
        // )
        const BODY: [u8; 7] = [
            0x00, // local decl count
            0x20, 0x00, // get_local 0
            0x41, 0x01, // i32.const 1
            0x6a, // i32.add
            0x0b, // end
        ];

        let mut trans = FuncTranslator::new();
        let flags = settings::Flags::new(settings::builder());
        let runtime = DummyEnvironment::new(
            isa::TargetFrontendConfig {
                default_call_conv: isa::CallConv::Fast,
                pointer_width: PointerWidth::U64,
            },
            ReturnMode::NormalReturns,
            false,
        );

        let mut ctx = Context::new();

        ctx.func.name = ir::ExternalName::testcase("small1");
        ctx.func.signature.params.push(ir::AbiParam::new(I32));
        ctx.func.signature.returns.push(ir::AbiParam::new(I32));

        trans
            .translate(&BODY, 0, &mut ctx.func, &mut runtime.func_env())
            .unwrap();
        debug!("{}", ctx.func.display(None));
        ctx.verify(&flags).unwrap();
    }

    #[test]
    fn small2() {
        // Same as above, but with an explicit return instruction.
        //
        // (func $small2 (param i32) (result i32)
        //     (return (i32.add (get_local 0) (i32.const 1)))
        // )
        const BODY: [u8; 8] = [
            0x00, // local decl count
            0x20, 0x00, // get_local 0
            0x41, 0x01, // i32.const 1
            0x6a, // i32.add
            0x0f, // return
            0x0b, // end
        ];

        let mut trans = FuncTranslator::new();
        let flags = settings::Flags::new(settings::builder());
        let runtime = DummyEnvironment::new(
            isa::TargetFrontendConfig {
                default_call_conv: isa::CallConv::Fast,
                pointer_width: PointerWidth::U64,
            },
            ReturnMode::NormalReturns,
            false,
        );
        let mut ctx = Context::new();

        ctx.func.name = ir::ExternalName::testcase("small2");
        ctx.func.signature.params.push(ir::AbiParam::new(I32));
        ctx.func.signature.returns.push(ir::AbiParam::new(I32));

        trans
            .translate(&BODY, 0, &mut ctx.func, &mut runtime.func_env())
            .unwrap();
        debug!("{}", ctx.func.display(None));
        ctx.verify(&flags).unwrap();
    }

    #[test]
    fn infloop() {
        // An infinite loop, no return instructions.
        //
        // (func $infloop (result i32)
        //     (local i32)
        //     (loop (result i32)
        //         (i32.add (get_local 0) (i32.const 1))
        //         (set_local 0)
        //         (br 0)
        //     )
        // )
        const BODY: [u8; 16] = [
            0x01, // 1 local decl.
            0x01, 0x7f, // 1 i32 local.
            0x03, 0x7f, // loop i32
            0x20, 0x00, // get_local 0
            0x41, 0x01, // i32.const 0
            0x6a, // i32.add
            0x21, 0x00, // set_local 0
            0x0c, 0x00, // br 0
            0x0b, // end
            0x0b, // end
        ];

        let mut trans = FuncTranslator::new();
        let flags = settings::Flags::new(settings::builder());
        let runtime = DummyEnvironment::new(
            isa::TargetFrontendConfig {
                default_call_conv: isa::CallConv::Fast,
                pointer_width: PointerWidth::U64,
            },
            ReturnMode::NormalReturns,
            false,
        );
        let mut ctx = Context::new();

        ctx.func.name = ir::ExternalName::testcase("infloop");
        ctx.func.signature.returns.push(ir::AbiParam::new(I32));

        trans
            .translate(&BODY, 0, &mut ctx.func, &mut runtime.func_env())
            .unwrap();
        debug!("{}", ctx.func.display(None));
        ctx.verify(&flags).unwrap();
    }
}
