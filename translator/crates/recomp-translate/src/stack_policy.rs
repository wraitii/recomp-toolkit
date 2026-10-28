use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{ensure, Context, Result};
use rk_core::facts::{StackLifetimeNominations, STACK_LIFETIME_FORMAT};
use rk_core::settings::{ReturnStackPolicy, Settings};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

fn digest(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(std::fs::read(path)?)))
}

pub(super) fn fingerprint(settings: &Settings) -> Result<Value> {
    let mut input: Value = serde_json::from_slice(&std::fs::read(&settings.input_path)?)?;
    if let Some(translate) = input["translate"].as_object_mut() {
        for key in [
            "return_stack_policy",
            "return_stack_functions",
            "return_stack_opt_out",
            "return_stack_facts",
        ] {
            translate.remove(key);
        }
    }
    let mut files = BTreeMap::new();
    let mut paths = vec![settings.code_map.clone()];
    paths.extend(
        [
            &settings.analysis_facts,
            &settings.indirect_targets,
            &settings.discovery_evidence,
            &settings.native_header,
            &settings.reloc,
        ]
        .into_iter()
        .flatten()
        .cloned(),
    );
    for key in ["x87_facts", "x87_mode_metadata"] {
        if let Some(path) = input["translate"][key].as_str() {
            paths.push(settings.game_dir.join(path));
        }
    }
    while let Some(path) = paths.pop() {
        if path.is_dir() {
            paths.extend(
                std::fs::read_dir(path)?
                    .map(|entry| entry.map(|entry| entry.path()))
                    .collect::<std::io::Result<Vec<_>>>()?,
            );
        } else {
            files.insert(path.to_string_lossy().into_owned(), digest(&path)?);
        }
    }
    Ok(
        json!({"executable_sha256": settings.sha256, "producer_sha256": digest(&std::env::current_exe()?)?,
        "configuration_sha256": format!("{:x}", Sha256::digest(serde_json::to_vec(&input)?)), "files": files}),
    )
}

pub(super) fn validate(settings: &Settings, inputs: &Value) -> Result<()> {
    if settings.return_stack_policy != ReturnStackPolicy::NormalCLifetime {
        return Ok(());
    }
    let path = settings
        .return_stack_facts
        .as_ref()
        .context("normal-c-lifetime requires return_stack_facts")?;
    let facts: StackLifetimeNominations = serde_json::from_slice(&std::fs::read(path)?)?;
    ensure!(
        facts.format == STACK_LIFETIME_FORMAT && facts.inputs == *inputs,
        "return-stack facts have an unsupported format or stale input fingerprints: {}",
        path.display()
    );
    for addr in settings
        .return_stack_functions
        .difference(&settings.return_stack_opt_out)
    {
        let function = format!("{addr:08x}");
        let plan = facts
            .functions
            .get(&function)
            .with_context(|| format!("missing return-stack nomination for {function}"))?;
        ensure!(
            plan.eligible_publications > 0
                && plan.counterexample_queries == 0
                && plan.supporting_queries > 0,
            "return-stack function {function} lacks a supported nomination or has a counterexample"
        );
    }
    Ok(())
}
