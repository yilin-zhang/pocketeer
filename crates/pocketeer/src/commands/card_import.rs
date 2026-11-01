use crate::human::{
    bytes, covers_summary, import_actions, skipped_summary, staged_footer, suggestion_footer,
};
use crate::{
    cli::CardImportArgs,
    commands::{
        Applied, Staged,
        library::{covers_after_import, suggest_standard_names},
        load_dat, stage_if, verdict,
    },
    context::Context,
    output::Outcome,
};
use pocketeer_core::{
    Error, Result,
    library::{
        card_import::{self, CardImportPlan, PlatformImport},
        covers::FetchReport,
        import::ImportOptions,
    },
    progress::Event,
    roms::index::CardRom,
};
use serde::Serialize;
use std::{fmt::Write, path::PathBuf};

impl Applied for Vec<PlatformImport> {
    fn failed(&self) -> usize {
        self.iter().map(|p| p.result.failed.len()).sum()
    }
}

#[derive(Debug, Serialize)]
pub struct ImportReport {
    #[serde(flatten)]
    pub staged: Staged<CardImportPlan, Vec<PlatformImport>>,
    /// Names asked for that aren't orphans on the card.
    pub not_found: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covers: Option<FetchReport>,
}

impl ImportReport {
    /// A plan shown in a TUI dialog before it is applied.
    pub fn preview(plan: CardImportPlan) -> Self {
        ImportReport {
            staged: Staged::preview(plan),
            not_found: Vec::new(),
            covers: None,
        }
    }
}

impl Outcome for ImportReport {
    fn error(&self) -> Option<Error> {
        verdict(
            self.staged.changes(),
            self.staged.failed() + self.not_found.len(),
        )
    }
}

/// Plans copying card-only ROMs into the library, plus the names asked for
/// that aren't orphans on the card.
pub fn plan_import(
    ctx: &Context,
    args: &CardImportArgs,
    progress: &mut dyn FnMut(Event),
) -> Result<(CardImportPlan, Vec<PathBuf>)> {
    let lib = ctx.library()?;
    let pocket = ctx.pocket()?;
    let dat = load_dat(args.dat.as_deref())?;
    let all = card_import::list(&lib, &pocket, args.platform.as_deref())?;
    let (roms, not_found) = if args.paths.is_empty() {
        (all, Vec::new())
    } else {
        let picked: Vec<CardRom> = all
            .iter()
            .filter(|r| args.paths.contains(&r.rel_path))
            .cloned()
            .collect();
        let missing = args
            .paths
            .iter()
            .filter(|n| !picked.iter().any(|r| r.rel_path == **n))
            .cloned()
            .collect();
        (picked, missing)
    };
    let mut plan = card_import::plan_import(
        &lib,
        &pocket,
        &roms,
        dat.as_ref(),
        &|id| ImportOptions {
            platform: id.into(),
            platform_name: ctx.platform_name(id),
            dat_only: args.dat_only,
        },
        &mut ctx.hash_cache(),
        progress,
    )?;
    for part in &mut plan.parts {
        suggest_standard_names(
            ctx,
            &lib,
            &part.platform,
            &mut part.plan,
            args.standard_names,
        )?;
    }
    Ok((plan, not_found))
}

/// Copies the planned ROMs into the library. The card itself is not changed.
pub fn apply_import(
    ctx: &Context,
    plan: &CardImportPlan,
    progress: &mut dyn FnMut(Event),
) -> Result<Vec<PlatformImport>> {
    card_import::apply_import(&mut ctx.library()?, &ctx.pocket()?, plan, progress)
}

pub fn import(
    ctx: &Context,
    args: &CardImportArgs,
    progress: &mut dyn FnMut(Event),
) -> Result<ImportReport> {
    let (plan, not_found) = plan_import(ctx, args, progress)?;
    let staged = stage_if(args.apply, not_found.is_empty(), plan, |plan| {
        apply_import(ctx, plan, progress)
    })?;
    let added = staged
        .result
        .as_ref()
        .map_or(0, |r| r.iter().map(|p| p.result.added).sum());
    let covers = covers_after_import(
        ctx,
        &ctx.library()?,
        None,
        (added, !args.no_covers),
        progress,
    );
    Ok(ImportReport {
        staged,
        not_found,
        covers,
    })
}

pub fn report(r: &ImportReport) -> String {
    let mut out = String::new();
    let p = &r.staged.plan;
    for part in &p.parts {
        let _ = writeln!(out, "[{}]", part.platform);
        let skipped = import_actions(&mut out, part.plan.actions.iter());
        if !skipped.is_empty() {
            let _ = writeln!(
                out,
                "  {}",
                skipped_summary(&skipped).trim_start_matches("; ")
            );
        }
    }
    for n in &r.not_found {
        let _ = writeln!(out, "✗ {}: not an unmatched ROM on this card", n.display());
    }
    let _ = writeln!(
        out,
        "{} ROM(s) to copy into the library (the card is not changed)",
        p.changes()
    );
    if r.staged.result.is_none() && p.changes() > 0 {
        suggestion_footer(
            &mut out,
            r.staged.mode,
            p.parts.iter().flat_map(|p| p.plan.actions.iter()),
        );
    }
    // Nothing is applied while a name is missing, so no footer then either.
    let changes = if r.not_found.is_empty() {
        p.changes()
    } else {
        0
    };
    staged_footer(
        &mut out,
        r.staged.mode,
        r.staged.result.as_ref(),
        changes,
        "Nothing to import.",
        |out, res| {
            for pi in res {
                let ir = &pi.result;
                let _ = writeln!(
                    out,
                    "✓ [{}] added {} ROM(s), {}{}",
                    pi.platform,
                    ir.added,
                    bytes(ir.bytes),
                    if ir.duplicates.is_empty() {
                        String::new()
                    } else {
                        format!("; {} already in the library, dropped", ir.duplicates.len())
                    }
                );
                for warning in &pi.warnings {
                    let _ = writeln!(out, "! {warning}");
                }
                for f in &ir.failed {
                    let _ = writeln!(out, "✗ {}: {}", f.source.display(), f.error.message);
                }
            }
            covers_summary(out, r.covers.as_ref(), r.staged.mode);
        },
    );
    out
}
