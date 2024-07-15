use anyhow::Result;
use swc_core::ecma::{transforms::base::hygiene::hygiene, visit::VisitMutWith};
use turbo_tasks::Vc;

use crate::{analyzer::graph::EvalContext, parse::ParseResult};

/// Designed after the renamer of esbuild.
///
/// This renamer renames non-top-level identifiers in parallel, and top-level
/// identifiers in series.

struct Renamer {}

async fn rename_module(module: Vc<ParseResult>) -> Result<Vc<ParseResult>> {
    match &*module.await? {
        ParseResult::Ok {
            program,
            comments,
            eval_context,
            globals,
            source_map,
        } => {
            let mut program = program.clone();

            program.visit_mut_with(&mut hygiene());

            let eval_context = EvalContext::new(
                &program,
                eval_context.unresolved_mark,
                eval_context.top_level_mark,
                false,
                None,
            );

            Ok(ParseResult::Ok {
                program,
                comments: comments.clone(),
                eval_context,
                globals: globals.clone(),
                source_map: source_map.clone(),
            }
            .cell())
        }
        _ => Ok(module),
    }
}