// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! This module translates the bytecode of a module to Boogie code.

use std::collections::{BTreeMap, BTreeSet};

use itertools::Itertools;
#[allow(unused_imports)]
use log::{debug, info};

use spec_lang::{
    env::{FunctionEnv, GlobalEnv, Loc, ModuleEnv, Parameter, StructEnv, TypeParameter},
    ty::{PrimitiveType, Type},
};
use stackless_bytecode_generator::{
    stackless_bytecode::{
        Bytecode::{self, *},
        TempIndex,
    },
    stackless_bytecode_generator::{StacklessFunction, StacklessModuleGenerator},
};

use crate::{
    boogie_helpers::{
        boogie_byte_blob, boogie_field_name, boogie_function_name, boogie_local_type,
        boogie_struct_name, boogie_struct_type_value, boogie_type_value, boogie_type_values,
        boogie_var_before_borrow, boogie_well_formed_check, WellFormedMode,
    },
    cli::Options,
    code_writer::CodeWriter,
    spec_translator::SpecTranslator,
};
use num::Zero;
use stackless_bytecode_generator::{
    lifetime_analysis::LifetimeAnalysis,
    stackless_bytecode::{AssignKind, BinaryOp, BranchCond, Constant, UnaryOp},
    stackless_control_flow_graph::StacklessControlFlowGraph,
};
use vm::file_format::CodeOffset;

pub struct BoogieTranslator<'env> {
    env: &'env GlobalEnv,
    writer: &'env CodeWriter,
    options: &'env Options,
}

pub struct ModuleTranslator<'env> {
    writer: &'env CodeWriter,
    options: &'env Options,
    module_env: ModuleEnv<'env>,
    stackless_bytecode: Vec<StacklessFunction>,
}

/// A struct encapsulating information which is threaded through translating the bytecodes of
/// a single function. This holds information which is relevant across multiple bytecode
/// instructions, like borrowing information and label offsets.
struct BytecodeContext<'l> {
    /// The stackless bytecode.
    code: &'l StacklessFunction,
    /// The bytecode offsets which require a label because they are branched to.
    branching_targets: BTreeSet<CodeOffset>,
    /// Set of mutable references, represented by local index. Used for debug tracking. Currently,
    /// after each mutation (either by an instruction or by call to a function with mutable
    /// parameters), we dump tracking info for all the variables in this set. This is a vast
    /// over-approximation; however, the execution trace visualizer will remove redundant
    /// entries, so it is more of a performance concern.
    mutable_refs: BTreeSet<usize>,
    /// A map from local indices  to the before borrow indices. Every root which is borrowed in this
    /// function has an entry here. For every BorrowLoc(d) | BorrowGlobal(d), we have an entry
    /// d -> i and construct variables $before_borrow_i to remember the value before borrowing,
    /// $before_borrow_i_ref to remember the reference, and $before_borrow_used to remember whether
    /// this was very used. We also track aliasing, i.e. if we have MoveLoc(d', d) we add d' -> i
    /// as well. The aliasing can lead to the same reference in the domain of this map pointing to
    /// multiple before indices. This results from branches (e.g. `if (b) r = ... else r = ...`).
    /// We use $before_borrow_i_used to track at execution time which branch has been taken.
    borrowed_to_before_index: BTreeMap<usize, BTreeSet<usize>>,
    /// A map of mut ref parameter indices to their before index. This works similar as
    /// `borrowed_to_before_index` except that those references are passed into a public function
    /// and not borrowed. They nevertheless behave similar as if they would have been borrowed
    /// at function entry and released at function exit.
    inherited_to_before_index: BTreeMap<usize, BTreeSet<usize>>,
    /// As determined by lifetime analysis, the set of references which become dead
    /// at a given bytecode offset.
    offset_to_dead_refs: BTreeMap<CodeOffset, BTreeSet<TempIndex>>,
}

impl<'l> BytecodeContext<'l> {
    fn new(code: &'l StacklessFunction) -> Self {
        Self {
            code,
            branching_targets: BTreeSet::new(),
            mutable_refs: BTreeSet::new(),
            borrowed_to_before_index: BTreeMap::new(),
            inherited_to_before_index: BTreeMap::new(),
            offset_to_dead_refs: BTreeMap::new(),
        }
    }
}

impl<'env> BoogieTranslator<'env> {
    pub fn new(env: &'env GlobalEnv, options: &'env Options, writer: &'env CodeWriter) -> Self {
        Self {
            env,
            writer,
            options,
        }
    }

    pub fn translate(&mut self) {
        // generate definitions for all modules.
        for module_env in self.env.get_modules() {
            ModuleTranslator::new(self, module_env).translate();
        }
    }
}

impl<'env> ModuleTranslator<'env> {
    /// Creates a new module translator. Calls the stackless bytecode generator and wraps
    /// result into the translator.
    fn new(parent: &'env BoogieTranslator, module: ModuleEnv<'env>) -> Self {
        let stackless_bytecode =
            StacklessModuleGenerator::new(&module, module.get_verified_module()).generate_module();
        Self {
            writer: parent.writer,
            options: parent.options,
            module_env: module,
            stackless_bytecode,
        }
    }

    /// Returns true if for the module no code should be produced because its already defined
    /// in the prelude.
    pub fn is_module_provided_by_prelude(&self) -> bool {
        let name = self.module_env.get_name();
        self.module_env.symbol_pool().string(name.name()).as_str() == "Vector"
            && name.addr().is_zero()
    }

    /// Translates this module.
    fn translate(&mut self) {
        if self.is_module_provided_by_prelude() {
            return;
        }
        info!(
            "translating module {}",
            self.module_env
                .get_name()
                .display(self.module_env.symbol_pool())
        );
        self.writer
            .set_location(&self.module_env.env.internal_loc());
        let spec_translator = SpecTranslator::new(self.writer, &self.module_env, false);
        spec_translator.translate_spec_vars();
        spec_translator.translate_spec_funs();
        self.translate_structs();
        self.translate_functions();
    }

    /// Translates all structs in the module.
    fn translate_structs(&self) {
        emitln!(
            self.writer,
            "\n\n// ** structs of module {}\n",
            self.module_env
                .get_name()
                .display(self.module_env.symbol_pool())
        );
        for struct_env in self.module_env.get_structs() {
            // Set the location to internal so we don't see locations of pack/unpack
            // in execution traces.
            self.writer
                .set_location(&self.module_env.env.internal_loc());
            self.translate_struct_type(&struct_env);
            if !struct_env.is_native() {
                self.translate_struct_accessors(&struct_env);
            }
        }
    }

    /// Translates the given struct.
    fn translate_struct_type(&self, struct_env: &StructEnv<'_>) {
        // Emit TypeName
        let struct_name = boogie_struct_name(&struct_env);
        emitln!(self.writer, "const unique {}: TypeName;", struct_name);

        // Emit FieldNames
        for (i, field_env) in struct_env.get_fields().enumerate() {
            let field_name = boogie_field_name(&field_env);
            emitln!(
                self.writer,
                "const {}: FieldName;\naxiom {} == {};",
                field_name,
                field_name,
                i
            );
        }

        // Emit TypeValue constructor function.
        let type_args = struct_env
            .get_type_parameters()
            .iter()
            .enumerate()
            .map(|(i, _)| format!("$tv{}: TypeValue", i))
            .join(", ");
        let mut field_types = String::from("EmptyTypeValueArray");
        for field_env in struct_env.get_fields() {
            field_types = format!(
                "ExtendTypeValueArray({}, {})",
                field_types,
                boogie_type_value(self.module_env.env, &field_env.get_type())
            );
        }
        let type_value = format!("StructType({}, {})", struct_name, field_types);
        if struct_name == "LibraAccount_T" {
            // Special treatment of well-known resource LibraAccount_T. The type_value
            // function is forward-declared in the prelude, here we only add an axiom for
            // it.
            emitln!(
                self.writer,
                "axiom {}_type_value() == {};",
                struct_name,
                type_value
            );
        } else {
            emitln!(
                self.writer,
                "function {}_type_value({}): TypeValue {{\n    {}\n}}",
                struct_name,
                type_args,
                type_value
            );
        }

        // Emit invariant functions.
        let spec_translator = SpecTranslator::new(self.writer, &struct_env.module_env, false);
        spec_translator.translate_invariant_functions(&struct_env);
    }

    /// Translates struct accessors (pack/unpack).
    fn translate_struct_accessors(&self, struct_env: &StructEnv<'_>) {
        // Pack function
        let type_args_str = struct_env
            .get_type_parameters()
            .iter()
            .map(|TypeParameter(s, _)| {
                format!("{}: TypeValue", s.display(struct_env.symbol_pool()))
            })
            .join(", ");
        let args_str = struct_env
            .get_fields()
            .map(|field_env| {
                format!(
                    "{}: Value",
                    field_env.get_name().display(struct_env.symbol_pool())
                )
            })
            .join(", ");
        emitln!(
            self.writer,
            "procedure {{:inline 1}} {}_pack($file_id: int, $byte_index: int, $var_idx: int, {}) returns ($struct: Value)\n{{",
            boogie_struct_name(struct_env),
            separate(vec![type_args_str, args_str.clone()], ", ")
        );
        self.writer.indent();
        let mut fields_str = String::from("EmptyValueArray");
        for field_env in struct_env.get_fields() {
            let type_check = boogie_well_formed_check(
                self.module_env.env,
                &format!("{}", field_env.get_name().display(struct_env.symbol_pool())),
                &field_env.get_type(),
                WellFormedMode::Default,
            );
            emit!(self.writer, &type_check);
            fields_str = format!(
                "ExtendValueArray({}, {})",
                fields_str,
                field_env.get_name().display(struct_env.symbol_pool())
            );
        }
        emitln!(self.writer, "$struct := Vector({});", fields_str);

        // Generate $DebugTrackLocal so we can see the constructed value before invariant
        // evaluation may abort.
        emitln!(
            self.writer,
            "if ($byte_index > 0) { assume $DebugTrackLocal($file_id, $byte_index, $var_idx, $struct); }"
        );

        // Insert invariant code.
        let spec_translator = SpecTranslator::new(self.writer, &struct_env.module_env, false);
        spec_translator.emit_pack_invariants(struct_env, "$struct");

        self.writer.unindent();
        emitln!(self.writer, "}\n");

        // Unpack function
        emitln!(
            self.writer,
            "procedure {{:inline 1}} {}_unpack($struct: Value) returns ({})\n{{",
            boogie_struct_name(struct_env),
            args_str
        );
        self.writer.indent();
        emitln!(self.writer, "assume is#Vector($struct);");
        for field_env in struct_env.get_fields() {
            emitln!(
                self.writer,
                "{} := $SelectField($struct, {});",
                field_env.get_name().display(struct_env.symbol_pool()),
                boogie_field_name(&field_env)
            );
            let type_check = boogie_well_formed_check(
                self.module_env.env,
                &format!("{}", field_env.get_name().display(struct_env.symbol_pool())),
                &field_env.get_type(),
                WellFormedMode::Default,
            );
            emit!(self.writer, &type_check);
        }

        // Insert invariant checking code.
        let spec_translator = SpecTranslator::new(self.writer, &struct_env.module_env, false);
        spec_translator.emit_unpack_invariants(struct_env, "$struct");

        self.writer.unindent();
        emitln!(self.writer, "}\n");
    }

    /// Translates all functions in the module.
    fn translate_functions(&self) {
        emitln!(
            self.writer,
            "\n\n// ** functions of module {}\n",
            self.module_env
                .get_name()
                .display(self.module_env.symbol_pool())
        );
        let mut num_fun_specified = 0;
        let mut num_fun = 0;
        for func_env in self.module_env.get_functions() {
            if !func_env.is_native() {
                num_fun += 1;
            }
            if !func_env.get_specification_on_decl().is_empty() && !func_env.is_native() {
                num_fun_specified += 1;
            }
            self.writer.set_location(&func_env.get_loc());
            self.translate_function(&func_env);
        }
        if num_fun > 0 {
            info!(
                "{} out of {} functions are specified in module {}",
                num_fun_specified,
                num_fun,
                self.module_env
                    .get_name()
                    .display(self.module_env.symbol_pool())
            );
        }
    }
}

impl<'env> ModuleTranslator<'env> {
    /// Translates the given function.
    fn translate_function(&self, func_env: &FunctionEnv<'_>) {
        if func_env.is_native() {
            if self.options.native_stubs {
                self.generate_function_sig(func_env, true);
                emit!(self.writer, ";");
                self.generate_function_spec(func_env);
                emitln!(self.writer);
            }
            return;
        }

        // generate inline function with function body
        self.generate_function_sig(func_env, true); // inlined version of function
        self.generate_function_spec(func_env);
        self.generate_inline_function_body(func_env);
        emitln!(self.writer);

        // If the function has no associated spec when the `only-verify-spec` flag is set,
        // the `_verify` version is not generated to skip verifying the function without spec.
        if self.options.only_verify_spec && func_env.get_specification_on_decl().is_empty() {
            return;
        }

        // generate the _verify version of the function which calls inline version for standalone
        // verification.
        self.generate_function_sig(func_env, false); // no inline
        self.generate_verify_function_body(func_env); // function body just calls inlined version
    }

    /// Return a string for a boogie procedure header.
    /// if inline = true, add the inline attribute and use the plain function name
    /// for the procedure name. Also inject pre/post conditions if defined.
    /// Else, generate the function signature without the ":inline" attribute, and
    /// append _verify to the function name.
    fn generate_function_sig(&self, func_env: &FunctionEnv<'_>, inline: bool) {
        let (args, rets) = self.generate_function_args_and_returns(func_env);
        if inline {
            emit!(
                self.writer,
                "procedure {{:inline 1}} {} ({}) returns ({})",
                boogie_function_name(func_env),
                args,
                rets,
            )
        } else {
            emit!(
                self.writer,
                "procedure {}_verify ({}) returns ({})",
                boogie_function_name(func_env),
                args,
                rets
            )
        }
    }

    /// Generate boogie representation of function args and return args.
    fn generate_function_args_and_returns(&self, func_env: &FunctionEnv<'_>) -> (String, String) {
        let args = func_env
            .get_type_parameters()
            .iter()
            .map(|TypeParameter(s, _)| format!("{}: TypeValue", s.display(func_env.symbol_pool())))
            .chain(func_env.get_parameters().iter().map(|Parameter(s, ty)| {
                format!(
                    "{}: {}",
                    s.display(func_env.symbol_pool()),
                    boogie_local_type(ty)
                )
            }))
            .join(", ");
        let rets = func_env
            .get_return_types()
            .iter()
            .enumerate()
            .map(|(i, ref s)| format!("$ret{}: {}", i, boogie_local_type(s)))
            .join(", ");
        (args, rets)
    }

    /// Return string for the function specification.
    fn generate_function_spec(&self, func_env: &FunctionEnv<'_>) {
        emitln!(self.writer);
        SpecTranslator::new(self.writer, &func_env.module_env, true).translate_conditions(func_env);
    }

    /// Return string for spec inside function implementation.
    fn generate_function_spec_inside_impl(&self, func_env: &FunctionEnv<'_>, offset: CodeOffset) {
        emitln!(self.writer);
        SpecTranslator::new_for_spec_in_impl(self.writer, func_env, true)
            .translate_conditions_inside_impl(func_env, offset);
    }

    /// Return string for body of verify function, which is just a call to the
    /// inline version of the function.
    fn generate_verify_function_body(&self, func_env: &FunctionEnv<'_>) {
        // Set the location to internal so it won't be counted for execution traces
        self.writer
            .set_location(&self.module_env.env.internal_loc());
        emitln!(self.writer, "{");
        self.writer.indent();

        // Generate assumes for top-level verification entry
        // (a) init prelude specific stuff.
        emitln!(self.writer, "call $InitVerification();");

        // (b) assume implicit preconditions.
        let spec_translator = SpecTranslator::new(self.writer, &func_env.module_env, false);
        spec_translator.assume_preconditions(func_env);

        // (c) assume reference parameters to be based on the Param(i) Location, ensuring
        // they are disjoint from all other references. This prevents aliasing and is justified as
        // follows:
        // - for mutual references, by their exclusive access in Move.
        // - for immutable references, by that mutation is not possible, and they are equivalent
        //   to some given but arbitrary value.
        for (i, Parameter(name, ty)) in func_env.get_parameters().iter().enumerate() {
            if ty.is_reference() {
                let name = func_env.symbol_pool().string(*name);
                emitln!(self.writer, "assume l#Reference({}) == Param({});", name, i);
                emitln!(self.writer, "assume size#Path(p#Reference({})) == 0;", name);
            }
        }

        // Generate call to inlined function.
        let args = func_env
            .get_type_parameters()
            .iter()
            .map(|TypeParameter(s, _)| format!("{}", s.display(func_env.symbol_pool())))
            .chain(
                func_env
                    .get_parameters()
                    .iter()
                    .map(|Parameter(s, _)| format!("{}", s.display(func_env.symbol_pool()))),
            )
            .join(", ");
        let rets = (0..func_env.get_return_count())
            .map(|i| format!("$ret{}", i))
            .join(", ");
        if rets.is_empty() {
            emitln!(
                self.writer,
                "call {}({});",
                boogie_function_name(func_env),
                args
            )
        } else {
            emitln!(
                self.writer,
                "call {} := {}({});",
                rets,
                boogie_function_name(func_env),
                args
            )
        }
        self.writer.unindent();
        emitln!(self.writer, "}");
        emitln!(self.writer);
    }

    /// This generates boogie code for everything after the function signature
    /// The function body is only generated for the "inline" version of the function.
    fn generate_inline_function_body(&self, func_env: &FunctionEnv<'_>) {
        let code = &self.stackless_bytecode[func_env.get_def_idx().0 as usize];

        // Construct context for bytecode translation.
        let mut context = BytecodeContext::new(code);

        // (a) Perform lifetime analysis.
        let cfg = StacklessControlFlowGraph::new(&code.code);
        context.offset_to_dead_refs =
            LifetimeAnalysis::analyze(&cfg, &code.code, &code.local_types);

        // (b) Walk over the bytecode and collect various context information.
        let mut before_borrow_counter = 0;
        for bytecode in code.code.iter() {
            match bytecode {
                Branch(target, _) => {
                    // Track that target requires a label.
                    context.branching_targets.insert(*target);
                }
                BorrowLoc(dst, ..) | BorrowGlobal(dst, ..) => {
                    let ty = self.get_local_type(func_env, *dst);
                    if ty.is_mutable_reference() {
                        // Track that we create a mutable reference here.
                        context.mutable_refs.insert(*dst);
                        if self.has_after_update_invariant(&ty) {
                            // Create a new borrow index.
                            context
                                .borrowed_to_before_index
                                .entry(*dst)
                                .or_insert_with(BTreeSet::new)
                                .insert(before_borrow_counter);
                            before_borrow_counter += 1;
                        }
                    }
                }
                BorrowField(dst, ..) => {
                    let ty = self.get_local_type(func_env, *dst);
                    if ty.is_mutable_reference() {
                        // Track that we create a mutable reference here.
                        context.mutable_refs.insert(*dst);
                    }
                }
                Assign(dst, src, AssignKind::Move) | Assign(dst, src, AssignKind::Store) => {
                    // Propagate information from src to dst.
                    if context.mutable_refs.contains(src) {
                        context.mutable_refs.insert(*dst);
                    }
                    if let Some(idx_set) = context.borrowed_to_before_index.get(src) {
                        // dst becomes an alias for src
                        let mut idx_set = idx_set.clone();
                        context
                            .borrowed_to_before_index
                            .entry(*dst)
                            .or_insert_with(BTreeSet::new)
                            .append(&mut idx_set);
                    }
                }
                _ => {}
            }
        }
        // (c) Walk over parameters and collect mutable references if they have update invariants.
        if func_env.is_public() {
            for (i, Parameter(_, ty)) in func_env.get_parameters().iter().enumerate() {
                if ty.is_mutable_reference() && self.has_after_update_invariant(ty) {
                    context
                        .inherited_to_before_index
                        .entry(i)
                        .or_insert_with(BTreeSet::new)
                        .insert(before_borrow_counter);
                    before_borrow_counter += 1;
                }
            }
        }

        // Be sure to set back location to the whole function definition as a default, otherwise
        // we may get unassigned code locations associated with condition locations.
        self.writer.set_location(&func_env.get_loc());

        emitln!(self.writer, "{");
        self.writer.indent();

        // Generate local variable declarations. They need to appear first in boogie.
        emitln!(self.writer, "// declare local variables");
        let num_args = func_env.get_parameters().len();
        for i in num_args..code.local_types.len() {
            let local_name = func_env.get_local_name(i);
            let local_type = &code.local_types[i];
            emitln!(
                self.writer,
                "var {}: {}; // {}",
                local_name.display(func_env.symbol_pool()),
                boogie_local_type(local_type),
                boogie_type_value(self.module_env.env, local_type)
            );
        }
        emitln!(self.writer, "var $tmp: Value;");
        emitln!(self.writer, "var $frame: int;");
        emitln!(self.writer, "var $saved_m: Memory;");
        let all_before_borrow_indices = context
            .borrowed_to_before_index
            .values()
            .chain(context.inherited_to_before_index.values())
            .flatten()
            .unique()
            .sorted()
            .collect_vec();
        for idx in &all_before_borrow_indices {
            // Declare before borrow variables.
            let name = boogie_var_before_borrow(**idx);
            emitln!(self.writer, "var {}: Value;", name);
            emitln!(self.writer, "var {}_ref: Reference;", name);
            emitln!(self.writer, "var {}_used: bool;", name);
        }

        emitln!(self.writer, "\n// initialize function execution");
        emitln!(self.writer, "assume !$abort_flag;");
        emitln!(self.writer, "$saved_m := $m;");
        emitln!(self.writer, "$frame := $local_counter;");

        emitln!(self.writer, "\n// process and type check arguments");
        let mode = if func_env.is_public() {
            // For public functions, we always include invariants in type assumptions for parameters,
            // even for mutable references.
            WellFormedMode::WithInvariant
        } else {
            WellFormedMode::Default
        };
        for i in 0..num_args {
            let local_name = func_env.get_local_name(i);
            let local_str = format!("{}", local_name.display(func_env.symbol_pool()));
            let local_type = &code.local_types[i];
            let type_check =
                boogie_well_formed_check(self.module_env.env, &local_str, local_type, mode);
            emit!(self.writer, &type_check);
            if !local_type.is_reference() {
                emitln!(
                    self.writer,
                    &self.update_and_track_local(func_env, func_env.get_loc(), i, &local_str)
                );
            } else {
                emitln!(
                    self.writer,
                    &self.track_local(func_env, func_env.get_loc(), i, &local_str)
                );
            }
        }

        // Initialize before_borrow_used variables to false.
        for idx in &all_before_borrow_indices {
            let name = boogie_var_before_borrow(**idx);
            emitln!(self.writer, "{}_used := false;", name);
        }

        if !context.inherited_to_before_index.is_empty() {
            emitln!(
                self.writer,
                "\n// save values and references for mutable ref parameters with invariants"
            );
            for (param_idx, before_idx_set) in context.inherited_to_before_index.iter() {
                self.save_and_enforce_before_update_set(&func_env, *param_idx, before_idx_set);
            }
        }

        emitln!(self.writer, "\n// increase the local counter");
        emitln!(
            self.writer,
            "$local_counter := $local_counter + {};",
            code.local_types.len()
        );

        emitln!(self.writer, "\n// bytecode translation starts here");

        // Generate bytecode
        for (offset, bytecode) in code.code.iter().enumerate() {
            self.translate_bytecode(func_env, &context, offset as CodeOffset, bytecode);
        }

        // Generate abort exit.
        let end_loc = func_env.get_loc().at_end();
        self.writer.set_location(&end_loc);
        self.writer.unindent();
        emitln!(self.writer, "Label_Abort:");
        self.writer.indent();
        emitln!(self.writer, "$abort_flag := true;");
        emitln!(self.writer, "$m := $saved_m;");
        for (i, ty) in func_env.get_return_types().iter().enumerate() {
            let ret_str = format!("$ret{}", i);
            if ty.is_reference() {
                emitln!(self.writer, "{} := DefaultReference;", &ret_str);
            } else {
                emitln!(self.writer, "{} := DefaultValue;", &ret_str);
            }
        }
        self.writer.unindent();
        emitln!(self.writer, "}");
    }

    /// Translates one bytecode instruction.
    fn translate_bytecode(
        &'env self,
        func_env: &FunctionEnv<'_>,
        ctx: &BytecodeContext<'_>,
        offset: u16,
        bytecode: &Bytecode,
    ) {
        // Set location of this code in the CodeWriter.
        let loc = func_env.get_bytecode_loc(offset);
        self.writer.set_location(&loc);
        emitln!(self.writer, "// {:?}", bytecode); // DEBUG

        // A boolean indicating whether we have evaluated invariants for refs going out of scope.
        let mut invariants_evaluated = false;

        // Helper function to get an Rc<String> for a local.
        let str_local = |idx: &usize| func_env.symbol_pool().string(func_env.get_local_name(*idx));

        // Helper functions to update a local including debug tracking.
        let update_and_track_local = |idx: usize, value: &str| {
            self.update_and_track_local(func_env, loc.clone(), idx, value)
        };

        // Helper functions to debug track a local.
        let track_local = |idx: usize, value: &str| {
            if idx < func_env.get_local_count() {
                self.track_local(func_env, loc.clone(), idx, value)
            } else {
                "".to_string()
            }
        };

        // Helper functions to debug track a return value.
        let track_return = |idx: usize| {
            self.track_local(
                func_env,
                loc.clone(),
                func_env.get_local_count() + idx,
                &format!("$ret{}", idx),
            )
        };

        // Helper function to debug track potential updates of references.
        let track_mutable_refs = |ctx: &BytecodeContext<'_>| {
            for idx in &ctx.mutable_refs {
                if *idx < func_env.get_local_count() {
                    let s = self.track_local(func_env, loc.clone(), *idx, str_local(&idx).as_str());
                    if !s.is_empty() {
                        emitln!(self.writer, &s);
                    }
                }
            }
            // Add reference parameter because we also want to debug track them when
            // references are written.
            for (idx, Parameter(_, ty)) in func_env.get_parameters().iter().enumerate() {
                if ty.is_mutable_reference() {
                    let s = self.track_local(func_env, loc.clone(), idx, str_local(&idx).as_str());
                    if !s.is_empty() {
                        emitln!(self.writer, &s);
                    }
                }
            }
        };

        // Helper to save a borrowed value before mutation starts.
        let save_borrowed_value = |ctx: &BytecodeContext<'_>, dest: &usize| {
            if let Some(idx_set) = ctx.borrowed_to_before_index.get(dest) {
                // Save the value before mutation happens. We also need to save the reference,
                // because the bytecode may reuse it for some other purpose, so we can construct
                // the after-value from it when the update invariant is executed.
                self.save_and_enforce_before_update_set(&func_env, *dest, idx_set);
            }
        };

        let propagate_abort = || {
            format!(
                "if ($abort_flag) {{\n  assume $DebugTrackAbort({}, {});\n  goto Label_Abort;\n}}",
                func_env.module_env.env.file_id_to_idx(loc.file_id()),
                loc.span().start(),
            )
        };

        // Insert labels for branching targets
        if ctx.branching_targets.contains(&offset) {
            self.writer.unindent();
            emitln!(self.writer, "Label_{}:", offset);
            self.writer.indent();
        }

        // Insert any specs at this bytecode offset
        self.generate_function_spec_inside_impl(func_env, offset);

        // Translate the bytecode instruction.
        match bytecode {
            Branch(target, BranchCond::Always) => {
                // In contrast to other instructions, we need to evaluate invariants BEFORE
                // the branch.
                self.enforce_invariants_for_dead_refs(func_env, ctx, offset);
                invariants_evaluated = true;
                emitln!(self.writer, "goto Label_{};", target)
            }
            Branch(target, BranchCond::True(idx)) => {
                self.enforce_invariants_for_dead_refs(func_env, ctx, offset);
                invariants_evaluated = true;
                emitln!(
                    self.writer,
                    "$tmp := $GetLocal($m, $frame + {});\nif (b#Boolean($tmp)) {{ goto Label_{}; }}",
                    idx,
                    target)
            }
            Branch(target, BranchCond::False(idx)) => {
                self.enforce_invariants_for_dead_refs(func_env, ctx, offset);
                invariants_evaluated = true;
                emitln!(
                    self.writer,
                    "$tmp := $GetLocal($m, $frame + {});\nif (!b#Boolean($tmp)) {{ goto Label_{}; }}",
                    idx,
                    target)
            }
            Assign(dest, src, _) => {
                if self.get_local_type(func_env, *dest).is_reference() {
                    emitln!(
                        self.writer,
                        "call {} := CopyOrMoveRef({});",
                        str_local(dest),
                        str_local(src)
                    );
                    let track = track_local(*dest, str_local(dest).as_str());
                    if !track.is_empty() {
                        emitln!(self.writer, &track);
                    }
                } else {
                    emitln!(
                        self.writer,
                        "call $tmp := CopyOrMoveValue($GetLocal($m, $frame + {}));",
                        src
                    );
                    emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
                }
            }
            BorrowLoc(dest, src) => {
                emitln!(
                    self.writer,
                    "call {} := BorrowLoc($frame + {});",
                    str_local(dest),
                    src,
                );
                emit!(
                    self.writer,
                    &boogie_well_formed_check(
                        self.module_env.env,
                        str_local(dest).as_str(),
                        &self.get_local_type(func_env, *dest),
                        // At the begining of a borrow, invariant holds.
                        WellFormedMode::WithInvariant,
                    )
                );
                save_borrowed_value(ctx, dest);
            }
            ReadRef(dest, src) => {
                emitln!(self.writer, "call $tmp := ReadRef({});", str_local(src));
                emit!(
                    self.writer,
                    &boogie_well_formed_check(
                        self.module_env.env,
                        "$tmp",
                        &self.get_local_type(func_env, *dest),
                        WellFormedMode::Default
                    )
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            WriteRef(dest, src) => {
                emitln!(
                    self.writer,
                    "call WriteRef({}, $GetLocal($m, $frame + {}));",
                    str_local(dest),
                    src,
                );
                track_mutable_refs(ctx);
            }
            FreezeRef(dest, src) => emitln!(
                self.writer,
                "call {} := FreezeRef({});",
                str_local(dest),
                str_local(src),
            ),
            Call(dests, mid, fid, type_actuals, args) => {
                let callee_env = self.module_env.env.get_module(*mid).into_function(*fid);
                // If this is a call to a function from another module, assume the module invariants
                // if any. This is correct because module invariants are guaranteed to hold whenever
                // code outside of the module is executed.
                if callee_env.module_env.get_id() != func_env.module_env.get_id()
                    && !callee_env.module_env.get_module_invariants().is_empty()
                {
                    let spec_translator =
                        SpecTranslator::new(self.writer, &callee_env.module_env, false);
                    spec_translator.assume_module_invariants(&callee_env);
                }
                // If this is a call to a public function within the same module,
                // and it has parameters which are mutated currently, we end mutating now,
                // enforcing the update invariant. At the end of the call, we restart mutating,
                // re-initializing the before value. This is reflecting the fact that mutable
                // reference parameters to public functions are consider frozen when passed
                // around outside of the module. So we need to mimic the same behavior for
                // calls to these function from inside the module.
                let frozen_ref_params = if callee_env.is_public()
                    && callee_env.module_env.get_id() == func_env.module_env.get_id()
                {
                    args.iter()
                        .filter_map(|arg| {
                            if let Some(before_idx_set) = ctx.borrowed_to_before_index.get(arg) {
                                Some((*arg, before_idx_set))
                            } else if let Some(before_idx_set) =
                                ctx.inherited_to_before_index.get(arg)
                            {
                                Some((*arg, before_idx_set))
                            } else {
                                None
                            }
                        })
                        .collect()
                } else {
                    BTreeMap::new()
                };
                // Now that we have calculated the frozen refs, enforce update invariants.
                for (idx, before_idx_set) in &frozen_ref_params {
                    let ty = self.get_local_type(func_env, *idx);
                    self.enforce_after_update_invariant_set(func_env, &ty, before_idx_set);
                }

                let mut dest_str = String::new();
                let mut args_str = String::new();
                let mut dest_type_assumptions = vec![];
                let mut tmp_assignments = vec![];

                args_str.push_str(&boogie_type_values(func_env.module_env.env, type_actuals));
                if !args_str.is_empty() && !args.is_empty() {
                    args_str.push_str(", ");
                }
                args_str.push_str(
                    &args
                        .iter()
                        .map(|arg_idx| {
                            if self.get_local_type(func_env, *arg_idx).is_reference() {
                                format!("{}", str_local(arg_idx))
                            } else {
                                format!("$GetLocal($m, $frame + {})", arg_idx)
                            }
                        })
                        .join(", "),
                );
                dest_str.push_str(
                    &dests
                        .iter()
                        .map(|dest_idx| {
                            let dest = str_local(dest_idx).to_string();
                            let dest_type = &self.get_local_type(func_env, *dest_idx);
                            dest_type_assumptions.push(boogie_well_formed_check(
                                self.module_env.env,
                                &dest,
                                dest_type,
                                WellFormedMode::Default,
                            ));
                            if !dest_type.is_reference() {
                                tmp_assignments.push(update_and_track_local(*dest_idx, &dest));
                            } else {
                                tmp_assignments.push(track_local(*dest_idx, &dest));
                            }
                            dest
                        })
                        .join(", "),
                );
                if dest_str == "" {
                    emitln!(
                        self.writer,
                        "call {}({});",
                        boogie_function_name(&callee_env),
                        args_str
                    );
                } else {
                    emitln!(
                        self.writer,
                        "call {} := {}({});",
                        dest_str,
                        boogie_function_name(&callee_env),
                        args_str
                    );
                }
                emitln!(self.writer, &propagate_abort());
                for s in &dest_type_assumptions {
                    emitln!(self.writer, s);
                }
                for s in &tmp_assignments {
                    emitln!(self.writer, s);
                }
                if callee_env.is_mutating() {
                    track_mutable_refs(ctx);
                }
                // After the call, save current value as before value and enforce before invariants.
                for (idx, before_idx_set) in &frozen_ref_params {
                    self.save_and_enforce_before_update_set(func_env, *idx, *before_idx_set);
                }
            }
            Pack(dest, mid, sid, type_actuals, fields) => {
                let struct_env = func_env.module_env.env.get_module(*mid).into_struct(*sid);
                let effective_dest = self.compute_effective_dest(func_env, ctx.code, offset, *dest);
                let track_args = if effective_dest < func_env.get_local_count() {
                    format!(
                        "{}, {}, {}",
                        func_env.module_env.env.file_id_to_idx(loc.file_id()),
                        loc.span().start(),
                        effective_dest,
                    )
                } else {
                    "0, 0, 0".to_string()
                };
                let args_str = type_actuals
                    .iter()
                    .map(|s| boogie_type_value(self.module_env.env, s))
                    .chain(
                        fields
                            .iter()
                            .map(|i| format!("$GetLocal($m, $frame + {})", i)),
                    )
                    .join(", ");
                emitln!(
                    self.writer,
                    "call $tmp := {}_pack({}, {});",
                    boogie_struct_name(&struct_env),
                    track_args,
                    args_str
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Unpack(dests, mid, sid, _, src) => {
                let struct_env = func_env.module_env.env.get_module(*mid).into_struct(*sid);
                let mut dests_str = String::new();
                let mut tmp_assignments = vec![];
                for dest in dests.iter() {
                    if !dests_str.is_empty() {
                        dests_str.push_str(", ");
                    }
                    let dest_str = str_local(dest);
                    let dest_type = &self.get_local_type(func_env, *dest);
                    dests_str.push_str(dest_str.as_str());
                    if !dest_type.is_reference() {
                        tmp_assignments.push(update_and_track_local(*dest, &dest_str));
                    } else {
                        tmp_assignments.push(track_local(*dest, &dest_str));
                    }
                }
                emitln!(
                    self.writer,
                    "call {} := {}_unpack($GetLocal($m, $frame + {}));",
                    dests_str,
                    boogie_struct_name(&struct_env),
                    src,
                );
                for s in &tmp_assignments {
                    emitln!(self.writer, s);
                }
            }
            BorrowField(dest, src, mid, sid, field_offset) => {
                let struct_env = func_env.module_env.env.get_module(*mid).into_struct(*sid);
                let field_env = &struct_env.get_field_by_offset(*field_offset);
                emitln!(
                    self.writer,
                    "call {} := BorrowField({}, {});",
                    str_local(dest),
                    str_local(src),
                    boogie_field_name(field_env)
                );
                emit!(
                    self.writer,
                    &boogie_well_formed_check(
                        self.module_env.env,
                        str_local(dest).as_str(),
                        &self.get_local_type(func_env, *dest),
                        WellFormedMode::Default
                    )
                );
            }
            Exists(dest, addr, mid, sid, type_actuals) => {
                let resource_type =
                    boogie_struct_type_value(self.module_env.env, *mid, *sid, type_actuals);
                emitln!(
                    self.writer,
                    "call $tmp := Exists($GetLocal($m, $frame + {}), {});",
                    addr,
                    resource_type
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            BorrowGlobal(dest, addr, mid, sid, type_actuals) => {
                let resource_type =
                    boogie_struct_type_value(self.module_env.env, *mid, *sid, type_actuals);
                emitln!(
                    self.writer,
                    "call {} := BorrowGlobal($GetLocal($m, $frame + {}), {});",
                    str_local(dest),
                    addr,
                    resource_type,
                );
                emit!(
                    self.writer,
                    &boogie_well_formed_check(
                        self.module_env.env,
                        str_local(dest).as_str(),
                        &self.get_local_type(func_env, *dest),
                        // At the beginning of a borrow, invariants always hold
                        WellFormedMode::WithInvariant,
                    )
                );
                emitln!(self.writer, &propagate_abort());
                save_borrowed_value(ctx, dest);
            }
            MoveToSender(src, mid, sid, type_actuals) => {
                let resource_type =
                    boogie_struct_type_value(self.module_env.env, *mid, *sid, type_actuals);
                emitln!(
                    self.writer,
                    "call MoveToSender({}, $GetLocal($m, $frame + {}));",
                    resource_type,
                    src,
                );
                emitln!(self.writer, &propagate_abort());
            }
            MoveFrom(dest, src, mid, sid, type_actuals) => {
                let resource_type =
                    boogie_struct_type_value(self.module_env.env, *mid, *sid, type_actuals);
                emitln!(
                    self.writer,
                    "call $tmp := MoveFrom($GetLocal($m, $frame + {}), {});",
                    src,
                    resource_type,
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
                emit!(
                    self.writer,
                    &boogie_well_formed_check(
                        self.module_env.env,
                        str_local(dest).as_str(),
                        &self.get_local_type(func_env, *dest),
                        WellFormedMode::Default
                    )
                );
                emitln!(self.writer, &propagate_abort());
            }
            Ret(rets) => {
                // In contrast to other instructions, we need to evaluate invariants BEFORE
                // the return.
                self.enforce_invariants_for_dead_refs(func_env, ctx, offset);
                self.enforce_invariants_for_inherited_refs(func_env, ctx);
                invariants_evaluated = true;
                for (i, r) in rets.iter().enumerate() {
                    if self.get_local_type(func_env, *r).is_reference() {
                        emitln!(self.writer, "$ret{} := {};", i, str_local(r));
                    } else {
                        emitln!(self.writer, "$ret{} := $GetLocal($m, $frame + {});", i, r);
                    }
                    emitln!(self.writer, &track_return(i));
                }
                emitln!(self.writer, "return;");
            }
            Load(idx, c) => {
                let value = match c {
                    Constant::Bool(true) => "Boolean(true)".to_string(),
                    Constant::Bool(false) => "Boolean(false)".to_string(),
                    Constant::U8(num) => format!("Integer({})", num),
                    Constant::U64(num) => format!("Integer({})", num),
                    Constant::U128(num) => format!("Integer({})", num),
                    Constant::Address(val) => format!("Address({})", val),
                    Constant::TxnSenderAddress => "$TxnSender($txn)".to_string(),
                    Constant::ByteArray(val) => boogie_byte_blob(val),
                };
                emitln!(self.writer, "$tmp := {};", value);
                emitln!(self.writer, &update_and_track_local(*idx, "$tmp"));
            }
            Unary(UnaryOp::CastU8, dest, src) => {
                emitln!(
                    self.writer,
                    "call $tmp := CastU8($GetLocal($m, $frame + {}));",
                    src
                );
                emitln!(self.writer, &propagate_abort());
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Unary(UnaryOp::CastU64, dest, src) => {
                emitln!(
                    self.writer,
                    "call $tmp := CastU64($GetLocal($m, $frame + {}));",
                    src
                );
                emitln!(self.writer, &propagate_abort());
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Unary(UnaryOp::CastU128, dest, src) => {
                emitln!(
                    self.writer,
                    "call $tmp := CastU128($GetLocal($m, $frame + {}));",
                    src
                );
                emitln!(self.writer, &propagate_abort());
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Unary(UnaryOp::Not, dest, operand) => {
                emitln!(
                    self.writer,
                    "call $tmp := Not($GetLocal($m, $frame + {}));",
                    operand
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Add, dest, op1, op2) => {
                let add_type = match self.get_local_type(func_env, *dest) {
                    Type::Primitive(PrimitiveType::U8) => "U8",
                    Type::Primitive(PrimitiveType::U64) => "U64",
                    Type::Primitive(PrimitiveType::U128) => "U128",
                    _ => unreachable!(),
                };
                emitln!(
                    self.writer,
                    "call $tmp := Add{}($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {}));",
                    add_type,
                    op1,
                    op2
                );
                emitln!(self.writer, &propagate_abort());
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Sub, dest, op1, op2) => {
                emitln!(
                    self.writer,
                    "call $tmp := Sub($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {}));",
                    op1,
                    op2
                );
                emitln!(self.writer, &propagate_abort());
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Mul, dest, op1, op2) => {
                let mul_type = match self.get_local_type(func_env, *dest) {
                    Type::Primitive(PrimitiveType::U8) => "U8",
                    Type::Primitive(PrimitiveType::U64) => "U64",
                    Type::Primitive(PrimitiveType::U128) => "U128",
                    _ => unreachable!(),
                };
                emitln!(
                    self.writer,
                    "call $tmp := Mul{}($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {}));",
                    mul_type,
                    op1,
                    op2
                );
                emitln!(self.writer, &propagate_abort());
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Div, dest, op1, op2) => {
                emitln!(
                    self.writer,
                    "call $tmp := Div($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {}));",
                    op1,
                    op2
                );
                emitln!(self.writer, &propagate_abort());
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Mod, dest, op1, op2) => {
                emitln!(
                    self.writer,
                    "call $tmp := Mod($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {}));",
                    op1,
                    op2
                );
                emitln!(self.writer, &propagate_abort());
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Lt, dest, op1, op2) => {
                emitln!(
                    self.writer,
                    "call $tmp := Lt($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {}));",
                    op1,
                    op2
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Gt, dest, op1, op2) => {
                emitln!(
                    self.writer,
                    "call $tmp := Gt($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {}));",
                    op1,
                    op2
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Le, dest, op1, op2) => {
                emitln!(
                    self.writer,
                    "call $tmp := Le($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {}));",
                    op1,
                    op2
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Ge, dest, op1, op2) => {
                emitln!(
                    self.writer,
                    "call $tmp := Ge($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {}));",
                    op1,
                    op2
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Or, dest, op1, op2) => {
                emitln!(
                    self.writer,
                    "call $tmp := Or($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {}));",
                    op1,
                    op2
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::And, dest, op1, op2) => {
                emitln!(
                    self.writer,
                    "call $tmp := And($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {}));",
                    op1,
                    op2
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Eq, dest, op1, op2) => {
                emitln!(
                    self.writer,
                    "$tmp := Boolean(IsEqual($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {})));",
                    op1,
                    op2
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::Neq, dest, op1, op2) => {
                emitln!(
                    self.writer,
                    "$tmp := Boolean(!IsEqual($GetLocal($m, $frame + {}), $GetLocal($m, $frame + {})));",
                    op1,
                    op2
                );
                emitln!(self.writer, &update_and_track_local(*dest, "$tmp"));
            }
            Binary(BinaryOp::BitOr, ..)
            | Binary(BinaryOp::BitAnd, ..)
            | Binary(BinaryOp::Xor, ..) => {
                emitln!(
                    self.writer,
                    "// bit operation not supported: {:?}",
                    bytecode
                );
            }
            Abort(_) => {
                // Below we introduce a dummy `if` for $DebugTrackAbort to ensure boogie creates
                // a execution trace entry for this statement.
                emitln!(
                    self.writer,
                    "if (true) {{ assume $DebugTrackAbort({}, {}); }}",
                    func_env.module_env.env.file_id_to_idx(loc.file_id()),
                    loc.span().start(),
                );
                emitln!(self.writer, "goto Label_Abort;")
            }
            Destroy(..) => {}
            _ => emitln!(self.writer, "// unimplemented instruction: {:?}", bytecode),
        }

        // Enforce invariants for references going out of scope after this instruction,
        // EXCEPT if it is a control flow statement (return, branch, etc.) For that one
        // we must enforce them before we actually jump, but after we did other
        // things.
        if !invariants_evaluated {
            self.enforce_invariants_for_dead_refs(func_env, ctx, offset);
        }

        emitln!(self.writer);
    }

    /// Save content of a ref into its before_borrow variable so we can enforce invariants once
    /// update finished. Also, if there is a before update invariant, evaluate it.
    fn save_and_enforce_before_update(
        &'env self,
        func_env: &FunctionEnv,
        ref_idx: usize,
        before_idx: usize,
    ) {
        let ref_name = func_env
            .symbol_pool()
            .string(func_env.get_local_name(ref_idx));
        let before_name = boogie_var_before_borrow(before_idx);
        emitln!(
            self.writer,
            "{} := $Dereference($m, {});",
            before_name,
            ref_name,
        );
        emitln!(self.writer, "{}_ref := {};", before_name, ref_name);
        emitln!(self.writer, "{}_used := true;", before_name);
        // Enforce the before update invariant (if any).
        self.enforce_before_update_invariant(
            func_env,
            &self.get_local_type(func_env, ref_idx),
            before_idx,
        );
    }

    /// Call `save_and_enforce_before_update` for a set.
    fn save_and_enforce_before_update_set(
        &'env self,
        func_env: &FunctionEnv,
        ref_idx: usize,
        before_idx_set: &BTreeSet<usize>,
    ) {
        for before_idx in before_idx_set {
            self.save_and_enforce_before_update(func_env, ref_idx, *before_idx);
        }
    }

    // Enforce invariants on references going out of scope
    fn enforce_invariants_for_dead_refs(
        &self,
        func_env: &FunctionEnv,
        context: &BytecodeContext,
        offset: CodeOffset,
    ) {
        if let Some(dead_refs) = context.offset_to_dead_refs.get(&(offset as u16)) {
            if !dead_refs.is_empty() {
                emitln!(
                    self.writer,
                    "// dead refs {:?}, borrowed_to_before_index {:?}",
                    dead_refs,
                    context.borrowed_to_before_index
                );
            }
            for ref_idx in dead_refs {
                let ref_idx = *ref_idx as usize;
                if let Some(idx_set) = context.borrowed_to_before_index.get(&ref_idx) {
                    let ty = self.get_local_type(func_env, ref_idx);
                    self.enforce_after_update_invariant_set(func_env, &ty, idx_set);
                }
            }
        }
    }

    /// Enforce invariants on references inherited as parameters.
    fn enforce_invariants_for_inherited_refs(
        &self,
        func_env: &FunctionEnv,
        context: &BytecodeContext,
    ) {
        for (ref_idx, before_idx_set) in &context.inherited_to_before_index {
            let ty = self.get_local_type(func_env, *ref_idx);
            self.enforce_after_update_invariant_set(func_env, &ty, before_idx_set);
        }
    }

    // Compute effective destination to enhance debug experience. This looks ahead
    // at the next instructions to detect a simple aliasing via copy or move and use that
    // destination as it might be a user variable whereas this instruction has a temporary
    // destination introduced by stackless bytecode transformation.
    // TODO: the stackless bytecode should optimize away unnecessary copy/moves, so we
    // don't need this. The below transformation is only correct for stackless code
    // of certain shape
    fn compute_effective_dest(
        &self,
        func_env: &FunctionEnv,
        code: &StacklessFunction,
        offset: CodeOffset,
        dest: usize,
    ) -> usize {
        if dest >= func_env.get_local_count() && offset as usize + 1 < code.code.len() {
            if let Pack(temp_dest, ..) = &code.code[offset as usize] {
                if let Assign(effective_dest, src, _) = &code.code[offset as usize + 1] {
                    if *src == *temp_dest {
                        return *effective_dest;
                    }
                }
            }
        }
        dest
    }

    /// Looks up the type of a local in the stackless bytecode representation.
    fn get_local_type(&self, func_env: &FunctionEnv<'_>, local_idx: usize) -> Type {
        self.stackless_bytecode[func_env.get_def_idx().0 as usize].local_types[local_idx].clone()
    }

    /// Determines whether this type has after-update invariants.
    fn has_after_update_invariant(&'env self, ty: &Type) -> bool {
        self.get_referred_struct(ty)
            .map(|struct_env| SpecTranslator::has_after_update_invariant(&struct_env))
            .unwrap_or(false)
    }

    /// If ty is a mutable reference to a struct, return its environment.
    fn get_referred_struct(&self, ty: &Type) -> Option<StructEnv<'_>> {
        if let Type::Reference(true, bt) = &ty {
            if let Type::Struct(module_idx, struct_idx, _) = bt.as_ref() {
                return Some(
                    self.module_env
                        .env
                        .get_module(*module_idx)
                        .into_struct(*struct_idx),
                );
            }
        }
        None
    }

    /// Enforce the invariant of an updated value before mutation starts. Does nothing if there
    /// is no before-update invariant.
    fn enforce_before_update_invariant(&self, _func_env: &FunctionEnv<'_>, ty: &Type, idx: usize) {
        if let Some(struct_env) = self.get_referred_struct(ty) {
            if SpecTranslator::has_before_update_invariant(&struct_env) {
                emitln!(
                    self.writer,
                    "call {}_before_update_inv({});",
                    boogie_struct_name(&struct_env),
                    boogie_var_before_borrow(idx)
                );
            }
        }
    }

    /// Enforce the invariant of an updated value after mutation ended. Does nothing if there is
    /// no after-update invariant.
    fn enforce_after_update_invariant(&self, _func_env: &FunctionEnv<'_>, ty: &Type, idx: usize) {
        if let Some(struct_env) = self.get_referred_struct(ty) {
            if SpecTranslator::has_after_update_invariant(&struct_env) {
                let name = &boogie_var_before_borrow(idx);
                emitln!(self.writer, "if ({}_used) {{", name);
                self.writer.indent();
                emitln!(
                    self.writer,
                    "call {}_after_update_inv({}, $Dereference($m, {}_ref));",
                    boogie_struct_name(&struct_env),
                    name,
                    name,
                );
                emitln!(self.writer, "{}_used := false;", name);
                self.writer.unindent();
                emitln!(self.writer, "}");
            }
        }
    }

    /// Calls enforce_after_update_invariant on a set of before-borrow indices.
    fn enforce_after_update_invariant_set(
        &self,
        func_env: &FunctionEnv<'_>,
        ty: &Type,
        idx_set: &BTreeSet<usize>,
    ) {
        for idx in idx_set {
            self.enforce_after_update_invariant(func_env, ty, *idx);
        }
    }

    /// Updates a local, injecting debug information if available.
    fn update_and_track_local(
        &self,
        func_env: &FunctionEnv<'_>,
        loc: Loc,
        idx: usize,
        value: &str,
    ) -> String {
        let update = format!("$m := $UpdateLocal($m, $frame + {}, {});", idx, value);
        if idx >= func_env.get_local_count() {
            // skip debug tracking for temporaries
            return update;
        }
        let debug_update = self.track_local(func_env, loc, idx, value);
        if !debug_update.is_empty() {
            format!("{}\n{}", update, debug_update)
        } else {
            update
        }
    }

    /// Generates an update of the model debug variable at given location.
    fn track_local(&self, func_env: &FunctionEnv<'_>, loc: Loc, idx: usize, value: &str) -> String {
        if idx >= func_env.get_local_count() {
            return "".to_string();
        }
        let ty = if idx < func_env.get_local_count() {
            func_env.get_local_type(idx)
        } else {
            func_env.get_return_types()[idx - func_env.get_local_count()].clone()
        };
        let value = if ty.is_reference() {
            format!("$Dereference($m, {})", value)
        } else {
            value.to_string()
        };
        format!(
            "if (true) {{ assume $DebugTrackLocal({}, {}, {}, {}); }}",
            func_env.module_env.env.file_id_to_idx(loc.file_id()),
            loc.span().start(),
            idx,
            value
        )
    }
}

/// Separates elements in vector, dropping empty ones.
fn separate(elems: Vec<String>, sep: &str) -> String {
    elems.iter().filter(|s| !s.is_empty()).join(sep)
}
