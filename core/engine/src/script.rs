//! Boa's implementation of ECMAScript's Scripts.
//!
//! This module contains the [`Script`] type, which represents a [**Script Record**][script].
//!
//! More information:
//!  - [ECMAScript reference][spec]
//!
//! [spec]: https://tc39.es/ecma262/#sec-scripts
//! [script]: https://tc39.es/ecma262/#sec-script-records

use std::path::{Path, PathBuf};

use rustc_hash::FxHashMap;

use boa_gc::{Finalize, Gc, GcRefCell, Trace};
use boa_parser::{Parser, Source, source::ReadChar};

use crate::{
    Context, HostDefined, JsResult, JsString, JsValue, Module, SpannedSourceText,
    bytecompiler::{ByteCompiler, global_declaration_instantiation_context},
    environments::EnvironmentStack,
    js_error, js_string,
    realm::Realm,
    spanned_source_text::SourceText,
    vm::{ActiveRunnable, CallFrame, CallFrameFlags, CodeBlock},
};

/// ECMAScript's [**Script Record**][spec].
///
/// [spec]: https://tc39.es/ecma262/#sec-script-records
#[derive(Clone, Trace, Finalize)]
pub struct Script {
    inner: Gc<Inner>,
}

impl std::fmt::Debug for Script {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Script")
            .field("realm", &self.inner.realm.addr())
            .field("phase", &self.inner.phase.borrow())
            .field("loaded_modules", &self.inner.loaded_modules)
            .finish()
    }
}

#[derive(Trace, Debug, Finalize)]
enum ScriptPhase {
    Ast(#[unsafe_ignore_trace] boa_ast::Script),
    Codeblock(Gc<CodeBlock>),
}

#[derive(Trace, Finalize)]
struct Inner {
    realm: Realm,
    phase: GcRefCell<ScriptPhase>,
    source_text: SourceText,
    loaded_modules: GcRefCell<FxHashMap<JsString, Module>>,
    host_defined: HostDefined,
    path: Option<PathBuf>,
}

impl Script {
    /// Gets the realm of this script.
    #[must_use]
    pub fn realm(&self) -> &Realm {
        &self.inner.realm
    }

    /// Returns the [`ECMAScript specification`][spec] defined [`\[\[HostDefined\]\]`][`HostDefined`] field of the [`Module`].
    ///
    /// [spec]: https://tc39.es/ecma262/#script-record
    #[must_use]
    pub fn host_defined(&self) -> &HostDefined {
        &self.inner.host_defined
    }

    /// Gets the loaded modules of this script.
    pub(crate) fn loaded_modules(&self) -> &GcRefCell<FxHashMap<JsString, Module>> {
        &self.inner.loaded_modules
    }

    /// Abstract operation [`ParseScript ( sourceText, realm, hostDefined )`][spec].
    ///
    /// Parses the provided `src` as an ECMAScript script, returning an error if parsing fails.
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-parse-script
    pub fn parse<R: ReadChar>(
        src: Source<'_, R>,
        realm: Option<Realm>,
        context: &mut Context,
    ) -> JsResult<Self> {
        let path = src.path().map(Path::to_path_buf);
        let mut parser = Parser::new(src);
        parser.set_identifier(context.next_parser_identifier());
        if context.is_strict() {
            parser.set_strict();
        }
        let scope = context.realm().scope().clone();
        let (mut code, source) = parser.parse_script_with_source(&scope, context.interner_mut())?;
        if !context.optimizer_options().is_empty() {
            context.optimize_statement_list(code.statements_mut());
        }

        let source_text = SourceText::new(source);

        Ok(Self {
            inner: Gc::new(Inner {
                realm: realm.unwrap_or_else(|| context.realm().clone()),
                phase: GcRefCell::new(ScriptPhase::Ast(code)),
                source_text,
                loaded_modules: GcRefCell::default(),
                host_defined: HostDefined::default(),
                path,
            }),
        })
    }

    /// Compiles the codeblock of this script.
    ///
    /// This is a no-op if this has been called previously.
    pub fn codeblock(&self, context: &mut Context) -> JsResult<Gc<CodeBlock>> {
        let cb = {
            let phase = self.inner.phase.borrow();
            let source = match &*phase {
                ScriptPhase::Codeblock(codeblock) => return Ok(codeblock.clone()),
                ScriptPhase::Ast(source) => source,
            };

            let mut annex_b_function_names = Vec::new();

            global_declaration_instantiation_context(
                &mut annex_b_function_names,
                source,
                self.inner.realm.scope(),
                context,
            )?;

            let spanned_source_text = SpannedSourceText::new_source_only(self.get_source());

            let mut compiler = ByteCompiler::new(
                js_string!("<main>"),
                source.strict(),
                false,
                self.inner.realm.scope().clone(),
                self.inner.realm.scope().clone(),
                false,
                false,
                context.interner_mut(),
                false,
                spanned_source_text,
                self.path().map(Path::to_owned).into(),
            );

            #[cfg(feature = "annex-b")]
            {
                compiler.annex_b_function_names = annex_b_function_names;
            }

            compiler.global_declaration_instantiation(source);
            compiler.compile_statement_list(source.statements(), true, false);

            Gc::new(compiler.finish())
        };

        *self.inner.phase.borrow_mut() = ScriptPhase::Codeblock(cb.clone());

        Ok(cb)
    }

    /// Returns a script that shares this one's compiled code but runs in `realm`.
    ///
    /// Parsing and compiling a large script is not cheap, and an embedder that
    /// gives every document a realm of its own — a browser, a sandbox per
    /// request — pays that price again for every one of them, for identical
    /// source. This compiles once and hands the result to as many realms as
    /// asked, without weakening the isolation between them: nothing of the
    /// running state is shared, only the instructions.
    ///
    /// The returned script has its own `[[LoadedModules]]` and
    /// [`\[\[HostDefined\]\]`][`HostDefined`], so a module loaded by one realm is
    /// not visible to another. The inline caches on the shared code are cleared,
    /// which is what stops one realm's measured behaviour from following the
    /// code into the next — see [`CodeBlock::clear_inline_caches`]. Caches are
    /// keyed on weak shape identity, so a stale entry could never have been
    /// *hit* by an object from a different realm; what accumulates instead is
    /// the megamorphic flag, which is permanent and would otherwise leave code
    /// reused across many realms unable to cache at all.
    ///
    /// The script must already be compiled: call [`Script::codeblock`] on it
    /// first, with the context it was parsed in. Taking no context here is
    /// deliberate. Compiling resolves the parser's symbols through *a* context's
    /// interner, and the only one that can resolve them correctly is the one
    /// that parsed the source, so an API that accepted a context here would
    /// accept the wrong one and silently compile a script full of unrelated
    /// identifiers.
    ///
    /// # Errors
    ///
    /// Returns a `TypeError` if the script has not been compiled yet.
    ///
    /// Also refuses a script whose *top level* declares `let`, `const` or
    /// `class`. Those become bindings in the global declarative scope, and the
    /// compiler records their position in the scope of the realm it compiled
    /// against; running such code in a second realm would leave that realm's own
    /// scope without them, so a script compiled in it afterwards would look for
    /// them on the global object and not find them. Top-level `var` and
    /// `function` are fine — they are instantiated by name on the global object,
    /// which is realm-independent, as is anything wrapped in a function.
    pub fn bind_to_realm(&self, realm: Realm) -> JsResult<Self> {
        let codeblock = match &*self.inner.phase.borrow() {
            ScriptPhase::Codeblock(codeblock) => codeblock.clone(),
            ScriptPhase::Ast(_) => {
                return Err(js_error!(
                    TypeError: "a script must be compiled before it can be bound to another \
                                realm; call `Script::codeblock` with the context it was parsed in"
                ));
            }
        };

        if !codeblock.global_lexs.is_empty() {
            return Err(js_error!(
                TypeError: "a script with top-level lexical declarations cannot be bound to \
                            another realm"
            ));
        }

        CodeBlock::clear_inline_caches(&codeblock);

        Ok(Self {
            inner: Gc::new(Inner {
                realm,
                phase: GcRefCell::new(ScriptPhase::Codeblock(codeblock)),
                source_text: self.inner.source_text.clone(),
                loaded_modules: GcRefCell::default(),
                host_defined: HostDefined::default(),
                path: self.inner.path.clone(),
            }),
        })
    }

    /// Evaluates this script and returns its result.
    ///
    /// Note that this won't run any scheduled promise jobs; you need to call [`Context::run_jobs`]
    /// on the context or [`JobExecutor::run_jobs`] on the provided queue to run them.
    ///
    /// [`JobExecutor::run_jobs`]: crate::job::JobExecutor::run_jobs
    pub fn evaluate(&self, context: &mut Context) -> JsResult<JsValue> {
        self.prepare_run(context)?;
        let record = context.run();

        context.vm.pop_frame();

        record.consume()
    }

    /// Evaluates this script and returns its result, periodically yielding to the executor
    /// in order to avoid blocking the current thread.
    ///
    /// This uses an implementation defined amount of "clock cycles" that need to pass before
    /// execution is suspended. See [`Script::evaluate_async_with_budget`] if you want to also
    /// customize this parameter.
    #[allow(clippy::future_not_send)]
    pub async fn evaluate_async(&self, context: &mut Context) -> JsResult<JsValue> {
        self.evaluate_async_with_budget(context, 256).await
    }

    /// Evaluates this script and returns its result, yielding to the executor each time `budget`
    /// number of "clock cycles" pass.
    ///
    /// Note that "clock cycle" is in quotation marks because we can't determine exactly how many
    /// CPU clock cycles a VM instruction will take, but all instructions have a "cost" associated
    /// with them that depends on their individual complexity. We'd recommend benchmarking with
    /// different budget sizes in order to find the ideal yielding time for your application.
    #[allow(clippy::future_not_send)]
    pub async fn evaluate_async_with_budget(
        &self,
        context: &mut Context,
        budget: u32,
    ) -> JsResult<JsValue> {
        self.prepare_run(context)?;

        let record = context.run_async_with_budget(budget).await;

        context.vm.pop_frame();

        record.consume()
    }

    fn prepare_run(&self, context: &mut Context) -> JsResult<()> {
        let codeblock = self.codeblock(context)?;

        let global_env = EnvironmentStack::new();
        context.vm.push_frame_with_stack(
            CallFrame::new(
                codeblock.clone(),
                Some(ActiveRunnable::Script(self.clone())),
                global_env,
                self.inner.realm.clone(),
            )
            .with_env_fp(0)
            .with_flags(CallFrameFlags::EXIT_EARLY),
            JsValue::undefined(),
            JsValue::null(),
        );

        self.realm().resize_global_env();

        context
            .global_declaration_instantiation(&codeblock)
            .inspect_err(|_| {
                context.vm.pop_frame();
            })?;

        Ok(())
    }

    pub(super) fn path(&self) -> Option<&Path> {
        self.inner.path.as_deref()
    }

    pub(super) fn get_source(&self) -> SourceText {
        self.inner.source_text.clone()
    }
}
