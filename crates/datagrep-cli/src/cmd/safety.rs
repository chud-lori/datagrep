use datagrep_api::safety::{Attestation, Requirement};
use datagrep_core::ProfileId;

use crate::cli::SafetyArgs;
use crate::context::Context;
use crate::exit::CliError;

// The engine decides; these flags are only how a terminal performs the ceremony it asked for.
pub(crate) fn clear(
    ctx: &Context,
    id: ProfileId,
    sql: &str,
    args: &SafetyArgs,
) -> Result<(), CliError> {
    let decision = ctx.core.evaluate_safety(id, sql)?;
    if decision.requirement == Requirement::None {
        return Ok(());
    }
    let Some(challenge) = &decision.challenge else {
        return Ok(());
    };

    let attestation = match (&args.confirm, args.acknowledge) {
        (Some(typed), _) => Attestation::TypedPhrase {
            typed: typed.clone(),
        },
        (None, true) => Attestation::Acknowledged,
        (None, false) => return Err(refusal(&decision)),
    };

    ctx.core
        .satisfy_safety(id, challenge, &attestation)
        .map_err(|e| CliError::usage(format!("{e}\n{}", how_to(&decision))))
}

fn refusal(decision: &datagrep_core::SafetyDecision) -> CliError {
    let listed: Vec<_> = decision
        .statements
        .iter()
        .filter(|s| s.requirement != Requirement::None)
        .map(|s| format!("  {}", s.text))
        .collect();
    CliError::usage(format!(
        "`{}` is at safety level {} — this needs {} first:\n{}\n{}",
        decision.profile,
        decision.level,
        decision.requirement,
        listed.join("\n"),
        how_to(decision)
    ))
}

fn how_to(decision: &datagrep_core::SafetyDecision) -> String {
    match decision.requirement {
        Requirement::Authenticate => format!(
            "Pass --confirm {} to authorise it, or lower the level with \
             `datagrep profiles safety {} <level>`.",
            decision.profile, decision.profile
        ),
        _ => format!(
            "Pass --acknowledge to run it anyway, or lower the level with \
             `datagrep profiles safety {} <level>`.",
            decision.profile
        ),
    }
}
