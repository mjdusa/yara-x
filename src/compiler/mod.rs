/*! Compiles YARA source code into binary form.

YARA rules must be compiled before they can be used for scanning data. This
module implements the YARA compiler.
*/
use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::path::Path;
use std::rc::Rc;
use string_interner::symbol::SymbolU32;
use string_interner::{DefaultBackend, StringInterner};
use walrus::ir::Instr::Unop;
use walrus::ir::UnaryOp;
use walrus::Module;

use crate::ast::*;
use crate::compiler::emit::emit_expr;
use crate::compiler::semcheck::semcheck;
use crate::parser::{Error as ParserError, Parser, SourceCode};
use crate::report::ReportBuilder;
use crate::warnings::Warning;
use crate::{modules, wasm, Struct};

#[doc(inline)]
pub use crate::compiler::errors::*;
use crate::symbol_table::{SymbolLookup, SymbolTable, TypeValue};
use crate::wasm::BuiltinFnTable;

mod emit;
mod errors;
mod semcheck;

#[cfg(test)]
mod tests;

/// A YARA compiler.
pub struct Compiler {
    colorize_errors: bool,

    report_builder: ReportBuilder,
    sym_tbl: SymbolTable,

    /// Pool that contains all the identifiers used in the rules. Each
    /// identifier appears only once, even if they are used by multiple
    /// rules. For example, the pool contains a single copy of the common
    /// identifier `$a`. Identifiers have an unique 32-bits ID that can
    /// be used for retrieving them from the pool.
    ident_pool: StringInterner<DefaultBackend<IdentID>>,

    /// Builder for creating the WebAssembly module that contains the code
    /// for all rule conditions.
    wasm_mod: wasm::ModuleBuilder,

    /// A vector with the all the rules that has been compiled. A [`RuleID`]
    /// is an index in this vector.
    rules: Vec<CompiledRule>,

    /// A vector with all the patterns from all the rules. A [`PatternID`]
    /// is an index in this vector.
    patterns: Vec<Pattern>,

    /// Warnings generated while compiling the rules.
    warnings: Vec<Warning>,
}

impl Compiler {
    /// Creates a new YARA compiler.
    pub fn new() -> Self {
        Self {
            colorize_errors: false,
            warnings: Vec::new(),
            rules: Vec::new(),
            patterns: Vec::new(),
            report_builder: ReportBuilder::new(),
            ident_pool: StringInterner::default(),
            wasm_mod: wasm::ModuleBuilder::new(),
            sym_tbl: SymbolTable::new(),
        }
    }

    /// Specifies whether the compiler should produce colorful error messages.
    ///
    /// Colorized error messages contain ANSI escape sequences that make them
    /// look nicer on compatible consoles. The default setting is `false`.
    pub fn colorize_errors(mut self, b: bool) -> Self {
        self.colorize_errors = b;
        self
    }

    /// Adds a YARA source code to be compiled.
    ///
    /// This function can be called multiple times.
    pub fn add_source<'src, S>(mut self, src: S) -> Result<Self, Error>
    where
        S: Into<SourceCode<'src>>,
    {
        self.report_builder.with_colors(self.colorize_errors);

        let src = src.into();

        let mut ast = Parser::new()
            .set_report_builder(&self.report_builder)
            .build_ast(src.clone())?;

        // Transfer to the compiler the warnings generated by the parser.
        self.warnings.append(&mut ast.warnings);

        for ns in ast.namespaces.iter() {
            // Process import statements. Checks that all imported modules
            // actually exist, and raise warnings in case of duplicated
            // imports.
            self.process_imports(&src, &ns.imports)?;

            // Iterate over the list of declared rules.
            for rule in ns.rules.iter() {
                // Create array with pairs (IdentID, PatternID) that describe
                // the patterns in a compiled rule.
                let pairs = if let Some(patterns) = &rule.patterns {
                    let mut pairs = Vec::with_capacity(patterns.len());
                    for pattern in patterns {
                        let ident_id = self
                            .ident_pool
                            .get_or_intern(pattern.identifier().as_str());

                        // The PatternID is the index of the pattern in
                        // `self.patterns`.
                        let pattern_id = self.patterns.len() as PatternID;

                        self.patterns.push(Pattern {});

                        pairs.push((ident_id, pattern_id));
                    }
                    pairs
                } else {
                    Vec::new()
                };

                let rule_id = self.rules.len() as RuleID;

                self.rules.push(CompiledRule {
                    ident: self
                        .ident_pool
                        .get_or_intern(rule.identifier.as_str()),
                    patterns: pairs,
                });

                let mut ctx = Context {
                    src: &src,
                    root_sym_tbl: &self.sym_tbl,
                    current_struct: None,
                    ident_pool: &self.ident_pool,
                    report_builder: &self.report_builder,
                    current_rule: self.rules.last().unwrap(),
                    wasm_imports: self.wasm_mod.imports(),
                    warnings: &mut self.warnings,
                };

                // Make sure that the condition is a boolean expression. This
                // traverses the condition's AST recursively checking that
                // all the condition is semantically valid.
                semcheck!(&mut ctx, Type::Bool, &rule.condition)?;

                // TODO: add rule name to declared identifiers.

                self.wasm_mod.main_fn().block(None, |block| {
                    let block_id = block.id();

                    // The RuleID is the argument to `rule_match`.
                    block.i32_const(rule_id as i32);

                    // Emit the code for the condition, which leave the
                    // condition's result in the stack.
                    emit_expr(&mut ctx, block, &rule.condition);

                    // If the condition's result is 0, jump out of the block
                    // and don't call the `rule_result` function.
                    block.unop(UnaryOp::I32Eqz);
                    block.br_if(block_id);

                    // Emit call instruction for calling `rule_match`.
                    block.call(ctx.wasm_imports.rule_match);
                });
            }
        }

        Ok(self)
    }

    pub fn build(self) -> Result<CompiledRules, Error> {
        // Finish building the WebAssembly module.
        let mut wasm_mod = self.wasm_mod.build();

        // Compile the WebAssembly module for the current platform. This
        // panics if the WebAssembly code is somehow invalid, which should
        // not happen, as the code is generated by YARA itself.
        let compiled_wasm_mod = wasmtime::Module::from_binary(
            &crate::wasm::ENGINE,
            wasm_mod.emit_wasm().as_slice(),
        )
        .unwrap();

        Ok(CompiledRules {
            compiled_wasm_mod,
            wasm_mod,
            ident_pool: self.ident_pool,
            patterns: Vec::new(),
            rules: self.rules,
        })
    }
}

impl Compiler {
    fn process_imports(
        &mut self,
        src: &SourceCode,
        imports: &[Import],
    ) -> Result<(), Error> {
        // Iterate over the list of imported modules.
        for import in imports.iter() {
            // Does the imported module actually exist? ...
            if let Some(module) =
                modules::BUILTIN_MODULES.get(import.module_name.as_str())
            {
                // ... if yes, add the module to the symbol table.
                self.sym_tbl.insert(
                    import.module_name.as_str(),
                    TypeValue::Struct(Rc::new(module)),
                );
            } else {
                // ... if no, that's an error.
                return Err(Error::CompileError(
                    CompileError::unknown_module(
                        &self.report_builder,
                        src,
                        import.module_name.to_string(),
                        import.span(),
                    ),
                ));
            }
        }

        Ok(())
    }
}

impl fmt::Debug for Compiler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Compiler")
    }
}

impl Default for Compiler {
    fn default() -> Self {
        Self::new()
    }
}

/// ID associated to each identifier in the identifiers pool.
type IdentID = SymbolU32;

/// ID associated to each pattern.
pub(crate) type PatternID = i32;

/// ID associated to each rule.
pub(crate) type RuleID = i32;

/// Structure that contains information and data structures required during the
/// the current compilation process.
struct Context<'a> {
    report_builder: &'a ReportBuilder,

    /// Symbol table that contains top-level symbols, like module names,
    /// and external variables.
    root_sym_tbl: &'a SymbolTable,

    /// Symbol table for the currently active structure. When this is None
    /// symbols are looked up in `root_sym_tbl` instead.
    current_struct: Option<Rc<dyn SymbolLookup + 'a>>,

    wasm_imports: BuiltinFnTable,

    /// Source code that is being compiled.
    src: &'a SourceCode<'a>,

    /// Rule that is being compiled.
    current_rule: &'a CompiledRule,

    /// Warnings generated during the compilation.
    warnings: &'a mut Vec<Warning>,

    /// Pool with identifiers used in the rules.
    ident_pool: &'a StringInterner<DefaultBackend<IdentID>>,
}

impl<'a> Context<'a> {
    /// Given an [`IdentID`] returns the identifier as `&str`.
    ///
    /// Panics if no identifier has the provided [`IdentID`].
    #[inline]
    fn resolve_ident(&self, ident_id: IdentID) -> &str {
        self.ident_pool.resolve(ident_id).unwrap()
    }

    /// Given a pattern identifier (e.g. `$a`) search for it in the current
    /// rule and return its [`PatternID`]. Panics if the current rule does not
    /// have the requested pattern.
    fn get_pattern_from_current_rule(&self, ident: &Ident) -> PatternID {
        for (ident_id, pattern_id) in &self.current_rule.patterns {
            if self.resolve_ident(*ident_id) == ident.as_str() {
                return *pattern_id;
            }
        }
        panic!(
            "rule `{}` does not have pattern `{}` ",
            self.resolve_ident(self.current_rule.ident),
            ident.as_str()
        );
    }
}

/// A set of YARA rules in compiled form.
///
/// This is the result from [`Compiler::proto`].
pub struct CompiledRules {
    /// Pool with identifiers used in the rules. Each identifier has its
    /// own [`IdentID`], which can be used for retrieving the identifier
    /// from the pool as a `&str`.
    ident_pool: StringInterner<DefaultBackend<IdentID>>,

    /// WebAssembly module containing the code for all rule conditions.
    wasm_mod: Module,

    /// WebAssembly module already compiled into native code for the current
    /// platform.
    compiled_wasm_mod: wasmtime::Module,

    /// Vector containing all the compiled rules. A [`RuleID`] is an index
    /// in this vector.
    rules: Vec<CompiledRule>,

    /// Vector with all the patterns used in the rules. This vector has not
    /// duplicated items, if two different rules use the "MZ" pattern, it
    /// appears in this list once. A [`PatternID`] is an index in this
    /// vector.
    patterns: Vec<Pattern>,
}

/// A compiled rule.
struct CompiledRule {
    /// The ID of the rule identifier in the identifiers pool.
    ident: IdentID,

    /// Vector with all the patterns defined by this rule.
    patterns: Vec<(IdentID, PatternID)>,
}

/// A pattern in the compiled rules.
struct Pattern {}

impl CompiledRules {
    /// Emits a `.wasm` file with the WebAssembly module generated for the
    /// rules.
    ///
    /// When YARA rules are compiled their conditions are translated to
    /// WebAssembly. This function emits the WebAssembly module that contains
    /// the code produced for these rules. The module can be inspected or
    /// disassembled with third-party [tooling](https://github.com/WebAssembly/wabt).
    pub fn emit_wasm_file<P>(&mut self, path: P) -> Result<(), Error>
    where
        P: AsRef<Path>,
    {
        Ok(self.wasm_mod.emit_wasm_file(path)?)
    }

    /// Returns the number of rules included in this instance of [`CompiledRules`].
    pub fn num_rules(&self) -> usize {
        self.rules.len()
    }

    pub(crate) fn compiled_wasm_mod(&self) -> &wasmtime::Module {
        &self.compiled_wasm_mod
    }
}
