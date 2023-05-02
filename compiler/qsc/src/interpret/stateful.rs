// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
mod tests;

use crate::{
    compile::{self, compile},
    error::WithSource,
};
use miette::Diagnostic;
use qsc_data_structures::index_map::IndexMap;
use qsc_eval::{
    debug::CallStack,
    output::Receiver,
    val::{GlobalId, Value},
    Env,
};
use qsc_frontend::{
    compile::{CompileUnit, PackageStore, Source, SourceMap},
    incremental::{self, Compiler, Fragment},
};
use qsc_hir::hir::{CallableDecl, Item, ItemKind, LocalItemId, PackageId, Stmt};
use qsc_passes::run_default_passes_for_fragment;
use std::sync::Arc;
use thiserror::Error;

use super::debug::format_call_stack;

#[derive(Clone, Debug, Diagnostic, Error)]
#[diagnostic(transparent)]
#[error(transparent)]
pub struct CompileError(WithSource<Source, compile::Error>);

#[derive(Clone, Debug, Diagnostic, Error)]
#[diagnostic(transparent)]
#[error(transparent)]
pub struct LineError(WithSource<Arc<str>, LineErrorKind>);

impl LineError {
    #[must_use]
    pub fn kind(&self) -> &LineErrorKind {
        self.0.error()
    }

    #[must_use]
    pub fn stack_trace(&self) -> &Option<String> {
        self.0.stack_trace()
    }
}

#[derive(Clone, Debug, Diagnostic, Error)]
#[diagnostic(transparent)]
pub enum LineErrorKind {
    #[error(transparent)]
    Compile(#[from] incremental::Error),
    #[error("runtime error")]
    Eval(#[from] qsc_eval::Error),
}

pub struct Interpreter {
    store: PackageStore,
    package: PackageId,
    compiler: Compiler,
    callables: IndexMap<LocalItemId, CallableDecl>,
    env: Env,
}

impl Interpreter {
    /// # Errors
    /// If the compilation of the standard library fails, an error is returned.
    /// If the compilation of the sources fails, an error is returned.
    pub fn new(std: bool, sources: SourceMap) -> Result<Self, Vec<CompileError>> {
        let mut store = PackageStore::new();
        let mut dependencies = Vec::new();
        if std {
            dependencies.push(store.insert(compile::std()));
        }

        let (unit, errors) = compile(&store, dependencies.iter().copied(), sources);
        if !errors.is_empty() {
            return Err(errors
                .into_iter()
                .map(|error| CompileError(WithSource::from_map(&unit.sources, error, None)))
                .collect());
        }

        dependencies.push(store.insert(unit));
        let package = store.insert(CompileUnit::default());
        let compiler = Compiler::new(&store, dependencies);
        Ok(Self {
            store,
            package,
            compiler,
            callables: IndexMap::new(),
            env: Env::with_empty_scope(),
        })
    }

    /// # Errors
    /// If the parsing of the line fails, an error is returned.
    /// If the compilation of the line fails, an error is returned.
    /// If there is a runtime error when interpreting the line, an error is returned.
    pub fn interpret_line(
        &mut self,
        receiver: &mut dyn Receiver,
        line: &str,
    ) -> Result<Value, Vec<LineError>> {
        let mut result = Value::unit();
        for mut fragment in self.compiler.compile_fragments(line) {
            run_default_passes_for_fragment(self.compiler.assigner_mut(), &mut fragment);
            match fragment {
                Fragment::Item(Item {
                    id,
                    kind: ItemKind::Callable(decl),
                    ..
                }) => {
                    self.callables.insert(id, decl);
                    result = Value::unit();
                }
                Fragment::Item(_) => {}
                Fragment::Stmt(stmt) => match self.eval_stmt(receiver, &stmt) {
                    Ok(value) => result = value,
                    Err((error, call_stack)) => {
                        let stack_trace = if call_stack.is_empty() {
                            None
                        } else {
                            Some(self.render_call_stack(&call_stack, &error))
                        };

                        return Err(vec![LineError(WithSource::new(
                            line.into(),
                            error.into(),
                            stack_trace,
                        ))]);
                    }
                },
                Fragment::Error(errors) => {
                    let source = line.into();
                    return Err(errors
                        .into_iter()
                        .map(|error| {
                            LineError(WithSource::new(Arc::clone(&source), error.into(), None))
                        })
                        .collect());
                }
            }
        }

        Ok(result)
    }

    fn eval_stmt(
        &mut self,
        receiver: &mut dyn Receiver,
        stmt: &Stmt,
    ) -> Result<Value, (qsc_eval::Error, CallStack)> {
        qsc_eval::eval_stmt(
            stmt,
            &|id| get_callable(&self.store, &self.callables, self.package, id),
            self.package,
            &mut self.env,
            receiver,
        )
    }

    fn render_call_stack(&self, call_stack: &CallStack, error: &dyn std::error::Error) -> String {
        format_call_stack(
            &self.store,
            &|id| get_callable(&self.store, &self.callables, self.package, id),
            call_stack,
            error,
        )
    }
}

fn get_callable<'a>(
    store: &'a PackageStore,
    callables: &'a IndexMap<LocalItemId, CallableDecl>,
    package: PackageId,
    id: GlobalId,
) -> Option<&'a CallableDecl> {
    if id.package == package {
        callables.get(id.item)
    } else {
        store.get(id.package).and_then(|unit| {
            let item = unit.package.items.get(id.item)?;
            if let ItemKind::Callable(callable) = &item.kind {
                Some(callable)
            } else {
                None
            }
        })
    }
}