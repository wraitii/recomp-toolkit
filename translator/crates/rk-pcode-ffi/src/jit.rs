use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::{c_char, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use recomp_translate::{facts, prepare_return_entries, EmitOptions, IrSummary, Job, Switches};
use rk_core::program::{EntryKind, Program};
use rk_core::settings::Settings;
use rk_output::Translation;

const INSTRUCTION_LIMIT: usize = 8192;
const MIN_HITS: u64 = 8;

type Compile = unsafe extern "C" fn(*const c_char, *const c_char) -> i32;

struct Manager {
    completed: Arc<Mutex<VecDeque<CString>>>,
    output: Arc<Mutex<Option<PathBuf>>>,
    requests: Arc<(Mutex<Requests>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Requests {
    hits: BTreeMap<u32, u64>,
    calls: BTreeMap<u32, u64>,
    pending: BTreeSet<(u64, u32)>,
    selected: BTreeSet<u32>,
    stopped: bool,
}

impl Requests {
    fn take_batch(queue: &(Mutex<Self>, Condvar)) -> Vec<u32> {
        let (mutex, wake) = queue;
        let mut requests = mutex.lock().unwrap();
        while requests.pending.is_empty() && !requests.stopped {
            requests = wake.wait(requests).unwrap();
        }
        let deadline = Instant::now() + Duration::from_millis(100);
        while requests.pending.len() < 16 && !requests.stopped {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            requests = wake.wait_timeout(requests, remaining).unwrap().0;
        }
        let mut entries = Vec::new();
        while !requests.stopped && entries.len() < 16 {
            let Some((_, target)) = requests.pending.pop_last() else {
                break;
            };
            requests.selected.insert(target);
            entries.push(target);
        }
        entries
    }
}

static MANAGER: OnceLock<Mutex<Manager>> = OnceLock::new();

fn start(base: u32, hash: String, compile: Compile) -> Mutex<Manager> {
    let requests = Arc::new((Mutex::new(Requests::default()), Condvar::new()));
    let worker_requests = Arc::clone(&requests);
    let completed = Arc::new(Mutex::new(VecDeque::new()));
    let worker_completed = Arc::clone(&completed);
    let output = Arc::new(Mutex::new(None));
    let worker_output = Arc::clone(&output);
    let worker = thread::spawn(move || {
        if let Err(error) = run(
            &worker_requests,
            base,
            &hash,
            compile,
            &worker_completed,
            &worker_output,
        ) {
            eprintln!("[recomp] jit: {error:#}");
        }
    });
    Mutex::new(Manager {
        completed,
        output,
        requests,
        worker: Some(worker),
    })
}

#[no_mangle]
pub unsafe extern "C" fn rp_jit_note(
    target: u32,
    is_call: i32,
    base: u32,
    hash: *const c_char,
    compile: Compile,
) {
    if hash.is_null() {
        return;
    }
    let manager = MANAGER.get_or_init(|| {
        start(
            base,
            CStr::from_ptr(hash).to_string_lossy().into_owned(),
            compile,
        )
    });
    let Ok(manager) = manager.lock() else {
        return;
    };
    let (mutex, wake) = &*manager.requests;
    let Ok(mut requests) = mutex.lock() else {
        return;
    };
    if requests.stopped {
        return;
    }
    if is_call != 0 {
        *requests.calls.entry(target).or_default() += 1;
    }
    let hits = requests.hits.entry(target).or_default();
    let previous = *hits;
    *hits += 1;
    let hits = *hits;
    if hits < MIN_HITS || requests.selected.contains(&target) {
        return;
    }
    requests.pending.remove(&(previous, target));
    requests.pending.insert((hits, target));
    if hits == MIN_HITS {
        wake.notify_one();
    }
}

#[no_mangle]
pub extern "C" fn rp_jit_take() -> *mut c_char {
    let Some(manager) = MANAGER.get() else {
        return std::ptr::null_mut();
    };
    let Ok(manager) = manager.lock() else {
        return std::ptr::null_mut();
    };
    let Ok(mut completed) = manager.completed.lock() else {
        return std::ptr::null_mut();
    };
    completed
        .pop_front()
        .map_or(std::ptr::null_mut(), CString::into_raw)
}

#[no_mangle]
pub unsafe extern "C" fn rp_jit_free_path(path: *mut c_char) {
    if !path.is_null() {
        drop(CString::from_raw(path));
    }
}

#[no_mangle]
pub unsafe extern "C" fn rp_jit_module_status(
    directory: *const c_char,
    status: *const c_char,
    reason: *const c_char,
    addresses: *const u32,
    calls: *const u64,
    handoffs: *const u64,
    resume_safe: i32,
    count: u32,
) {
    let entries: Vec<_> = (0..count as usize)
        .map(|i| {
            serde_json::json!({
                "target": format!("{:08x}", *addresses.add(i)), "native_calls": *calls.add(i),
                "branch_handoffs": *handoffs.add(i)
            })
        })
        .collect();
    let state = serde_json::json!({"status": CStr::from_ptr(status).to_string_lossy(),
        "reason": if reason.is_null() { None } else { Some(CStr::from_ptr(reason).to_string_lossy()) },
        "branch_handoff_supported": resume_safe != 0,
        "branch_handoff_restriction": if resume_safe != 0 { None } else { Some("batch establishes SEH checkpoints") },
        "entries": entries});
    let path =
        PathBuf::from(CStr::from_ptr(directory).to_string_lossy().as_ref()).join("runtime.json");
    if let Err(error) = std::fs::write(&path, serde_json::to_vec_pretty(&state).unwrap_or_default())
    {
        eprintln!("[recomp] jit: cannot write {}: {error}", path.display());
    }
}

#[no_mangle]
pub extern "C" fn rp_jit_shutdown() {
    let Some(manager) = MANAGER.get() else {
        return;
    };
    let Ok(mut manager) = manager.lock() else {
        return;
    };
    let (mutex, wake) = &*manager.requests;
    let mut requests = mutex.lock().unwrap();
    requests.stopped = true;
    wake.notify_one();
    let encounters: Vec<_> = requests
        .hits
        .iter()
        .map(|(&target, &hits)| {
            let calls = requests.calls.get(&target).copied().unwrap_or(0);
            serde_json::json!({"target": format!("{target:08x}"), "hits": hits,
            "calls": calls, "branches": hits - calls,
            "queued": hits >= MIN_HITS, "selected": requests.selected.contains(&target)})
        })
        .collect();
    drop(requests);
    let worker = manager.worker.take();
    let output = Arc::clone(&manager.output);
    let completed = Arc::clone(&manager.completed);
    drop(manager);
    if let Some(worker) = worker {
        let _ = worker.join();
    }
    if let Ok(completed) = completed.lock() {
        for directory in completed.iter() {
            let path = PathBuf::from(directory.to_string_lossy().as_ref()).join("runtime.json");
            let reason = if std::env::var("RECOMP_JIT_EXEC").as_deref() == Ok("off") {
                "native execution disabled"
            } else {
                "guest stopped before publication"
            };
            let _ = std::fs::write(
                path,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "status": "not_published", "reason": reason, "entries": []
                }))
                .unwrap_or_default(),
            );
        }
    }
    if let Ok(output) = output.lock() {
        if let Some(output) = output.as_ref() {
            let _ = std::fs::write(
                output.join("encounters.json"),
                serde_json::to_vec_pretty(&encounters).unwrap_or_default(),
            );
        }
    };
}

fn run(
    requests: &(Mutex<Requests>, Condvar),
    base: u32,
    hash: &str,
    compile: Compile,
    completed: &Mutex<VecDeque<CString>>,
    output: &Mutex<Option<PathBuf>>,
) -> anyhow::Result<()> {
    let input = std::env::var_os("RECOMP_JIT_INPUT")
        .ok_or_else(|| anyhow::anyhow!("missing RECOMP_JIT_INPUT"))?;
    let out = std::env::var_os("RECOMP_JIT_OUT")
        .ok_or_else(|| anyhow::anyhow!("missing RECOMP_JIT_OUT"))?;
    let configured_limit = std::env::var("RECOMP_JIT_MAX_INSNS")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(INSTRUCTION_LIMIT);
    let instruction_limit = if configured_limit == 0 {
        usize::MAX
    } else {
        configured_limit
    };
    let mut settings = Settings::load(Path::new(&input), None)?;
    if settings.base != base || settings.sha256 != hash {
        anyhow::bail!("runtime image does not match translation input");
    }
    settings.direct_variants = false;
    settings.call_contracts = false;
    settings.analysis_facts = None;
    settings.indirect_targets = None;
    let loaded = rk_image::load(&settings)?;
    let mut ranges: Vec<_> = loaded
        .program
        .spans
        .iter()
        .map(|&(start, end, _)| (start, end))
        .collect();
    let image = loaded.program.image;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let out = PathBuf::from(out).join(format!("{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&out)?;
    *output
        .lock()
        .map_err(|_| anyhow::anyhow!("output lock poisoned"))? = Some(out.clone());
    let mut decisions = BTreeMap::new();
    eprintln!("[recomp] jit: writing to {}", out.display());
    let mut covered = BTreeSet::new();
    let mut pending = VecDeque::new();
    let mut batch_number = 0;
    loop {
        if requests.0.lock().unwrap().stopped {
            break;
        }
        let retry = !pending.is_empty();
        let mut entries = if let Some(entry) = pending.pop_front() {
            vec![entry]
        } else {
            Requests::take_batch(requests)
        };
        if let Ok(selected) = std::env::var("RECOMP_JIT_TARGET") {
            let selected = u32::from_str_radix(selected.trim_start_matches("0x"), 16)?;
            entries.retain(|entry| *entry == selected);
        }
        entries.retain(|entry| {
            let reason = if !image.is_exec(*entry) {
                Some("non_executable")
            } else if covered.contains(entry) {
                Some("compiled_entry")
            } else if ranges.iter().any(|&(lo, hi)| lo <= *entry && *entry < hi) {
                Some("mapped_or_compiled_range")
            } else {
                None
            };
            if let Some(reason) = reason {
                decisions.insert(
                    format!("{entry:08x}"),
                    serde_json::json!({"status": "skipped", "reason": reason}),
                );
            }
            reason.is_none()
        });
        std::fs::write(
            out.join("requests.json"),
            serde_json::to_vec_pretty(&decisions)?,
        )?;
        entries.sort_unstable();
        entries.dedup();
        if entries.is_empty() {
            continue;
        }
        batch_number += 1;
        let selection = {
            let requests = requests.0.lock().unwrap();
            serde_json::json!({"policy": if retry { "single_request_retry" } else { "highest_hits" },
                "pending_targets": requests.pending.len(),
                "targets": entries.iter().map(|entry| serde_json::json!({
                    "target": format!("{entry:08x}"), "hits": requests.hits[entry]
                })).collect::<Vec<_>>()})
        };
        let result = emit(
            &settings,
            &image,
            &entries,
            &ranges,
            &out,
            batch_number,
            instruction_limit,
        );
        match result {
            Ok((emitted, new_ranges, limited)) => {
                let dir = out.join(format!("batch-{batch_number:06}"));
                let mut compiled = false;
                let mut compile_status = "ineligible";
                let mut exit_code = None;
                if dir.join("module.c").is_file() {
                    let input_c = CString::new(Path::new(&input).to_string_lossy().as_bytes())?;
                    let dir_c = CString::new(dir.to_string_lossy().as_bytes())?;
                    let status = unsafe { compile(input_c.as_ptr(), dir_c.as_ptr()) };
                    exit_code = Some(status);
                    if status == 0 {
                        compiled = true;
                        compile_status = "compiled_pending_publication";
                        if let Ok(mut queue) = completed.lock() {
                            queue.push_back(dir_c);
                        }
                    } else {
                        compile_status = "compile_failed";
                        eprintln!(
                            "[recomp] jit: batch {batch_number}: compilation failed; interpreting"
                        );
                    }
                }
                let manifest_path = dir.join("jit.json");
                let mut manifest: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
                manifest["compilation"] =
                    serde_json::json!({"status": compile_status, "exit_code": exit_code});
                manifest["selection"] = selection;
                std::fs::write(manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
                for entry in &entries {
                    decisions.insert(
                        format!("{entry:08x}"),
                        serde_json::json!({"status": compile_status, "batch": batch_number}),
                    );
                }
                if limited && !retry && entries.len() > 1 {
                    pending.extend(entries.iter().copied());
                    eprintln!(
                        "[recomp] jit: batch {batch_number}: retrying {} requests separately",
                        entries.len()
                    );
                }
                if compiled {
                    covered.extend(emitted);
                    ranges.extend(new_ranges);
                }
            }
            Err(error) => {
                for entry in &entries {
                    decisions.insert(format!("{entry:08x}"), serde_json::json!({"status": "recovery_or_emission_failed", "batch": batch_number, "error": format!("{error:#}")}));
                }
                eprintln!("[recomp] jit: batch {batch_number}: {error:#}");
                let _ = std::fs::write(
                    out.join(format!("batch-{batch_number:06}.error")),
                    format!("{error:#}\n"),
                );
            }
        }
        std::fs::write(
            out.join("requests.json"),
            serde_json::to_vec_pretty(&decisions)?,
        )?;
    }
    Ok(())
}

fn emit(
    settings: &Settings,
    image: &rk_core::program::Image,
    requests: &[u32],
    excluded: &[(u32, u32)],
    out: &Path,
    batch: u32,
    instruction_limit: usize,
) -> anyhow::Result<(BTreeSet<u32>, Vec<(u32, u32)>, bool)> {
    let mut program = Program::new(image.clone());
    let discovered = rk_discover::discover_jit_from_entries_excluding(
        &program,
        requests,
        instruction_limit,
        excluded,
    )?;
    let owners: Vec<_> = discovered.functions.iter().map(|(&owner, spans)| {
        serde_json::json!({"owner": format!("{owner:08x}"),
            "instructions": spans.iter().map(|span| span.lengths.as_ref().map_or(0, Vec::len)).sum::<usize>(),
            "bytes": spans.iter().map(|span| span.size as u64).sum::<u64>()})
    }).collect();
    let recovery = serde_json::json!({"mode": "jit_control_flow", "follow_calls": false,
        "seed_code_pointers": false, "function_count": discovered.functions.len(),
        "instructions": discovered.instruction_lengths.len(),
        "bytes": discovered.instruction_lengths.values().map(|&size| u64::from(size)).sum::<u64>(),
        "external_call_sites": discovered.report.external_calls.len(), "owners": owners});
    eprintln!("[recomp] jit: batch {batch}: local control flow from {} targets: {} instructions, {} regions, {} direct calls left to host dispatch",
        requests.iter().map(|entry| format!("{entry:08x}")).collect::<Vec<_>>().join(","),
        discovered.instruction_lengths.len(), discovered.functions.len(), discovered.report.external_calls.len());
    if discovered.instruction_lengths.len() >= instruction_limit {
        let dir = out.join(format!("batch-{batch:06}"));
        std::fs::create_dir_all(&dir)?;
        let ranges: Vec<_> = discovered.functions.iter().flat_map(|(&owner, spans)| spans.iter().map(move |span| serde_json::json!({"owner": format!("{owner:08x}"), "start": format!("{:08x}", span.start), "size": span.size}))).collect();
        let manifest = serde_json::json!({
            "entries": requests.iter().map(|entry| format!("{entry:08x}")).collect::<Vec<_>>(),
            "image_sha256": settings.sha256, "image_base": format!("{:08x}", image.base),
            "instruction_limit": instruction_limit, "instruction_count": discovered.instruction_lengths.len(),
            "recovery_limit_reached": true, "eligibility": {"eligible": false, "reasons": ["instruction_limit"]},
            "ranges": ranges, "discovery": discovered.report, "emitted": 0, "trapping": 0, "rejections": {},
            "recovery": recovery,
            "emission_skipped": "recovery budget exhausted"
        });
        std::fs::write(dir.join("jit.json"), serde_json::to_vec_pretty(&manifest)?)?;
        eprintln!("[recomp] jit: batch {batch}: instruction limit ({instruction_limit}); skipping analysis and emission");
        return Ok((BTreeSet::new(), Vec::new(), true));
    }
    if discovered.functions.is_empty() {
        anyhow::bail!("no requested code was recovered");
    }
    rk_discover::merge(&mut program, &discovered)?;
    let native = BTreeSet::new();
    let job = Job {
        program: &program,
        settings,
        native: &native,
    };
    let mut analysis = job.lift();
    let summary = IrSummary::new(&analysis);
    let facts = facts::load(settings, &program, &analysis)?;
    let global = job.global(&analysis, &facts)?;
    let extra_entries = prepare_return_entries(&mut analysis, &global.returns);
    let mut switches = Switches::default();
    switches.direct = false;
    let (bodies, stats, direct_variants) = job.emit(
        &analysis,
        None,
        &global,
        &facts,
        EmitOptions {
            switches,
            ..EmitOptions::default()
        },
    )?;
    for (addr, owner) in extra_entries {
        program.entries.entry(addr).or_insert(EntryKind::Discovered);
        let entries = program.interior_by_function.entry(owner).or_default();
        if !entries.contains(&addr) {
            entries.push(addr);
            entries.sort_unstable();
        }
    }
    let dir = out.join(format!("batch-{batch:06}"));
    let translation = Translation {
        settings,
        program: &program,
        bodies,
        preludes: rk_emit::preludes(),
        call_returns: summary.call_returns,
        call_facts: facts.runtime_call_facts,
        cw_effects: facts.cw_effects,
        host_methods: facts.host_calls.methods,
        direct_variants,
    };
    rk_output::write(&dir, &translation)?;
    let limit_reached = discovered.instruction_lengths.len() >= instruction_limit;
    let mut reasons = Vec::new();
    if limit_reached {
        reasons.push("instruction_limit");
    }
    if stats.trapping != 0 {
        reasons.push("trapping_bodies");
    }
    if !stats.rejections.is_empty() {
        reasons.push("emission_rejections");
    }
    if !program.tables.is_empty() {
        reasons.push("unvalidated_switch_tables");
    }
    let eligible = reasons.is_empty();
    if eligible {
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for (&addr, &size) in &discovered.instruction_lengths {
            if let Some((start, length)) = ranges.last_mut() {
                if *start + *length == addr {
                    *length += u32::from(size);
                    continue;
                }
            }
            ranges.push((addr, u32::from(size)));
        }
        rk_output::jit::write(&dir, &translation, &ranges)?;
    } else {
        eprintln!(
            "[recomp] jit: batch {batch}: ineligible: {} ({} instructions, {} switch tables)",
            reasons.join(", "),
            discovered.instruction_lengths.len(),
            program.tables.len()
        );
    }
    let entries: BTreeSet<u32> = program.entries.keys().copied().collect();
    let new_ranges: Vec<_> = program
        .functions
        .values()
        .flat_map(|f| f.spans.iter().map(|s| (s.start, s.end())))
        .collect();
    let ranges: Vec<_> = program.functions.values().flat_map(|f| f.spans.iter().map(move |s| serde_json::json!({"owner": format!("{:08x}", f.addr), "start": format!("{:08x}", s.start), "size": s.size}))).collect();
    let manifest = serde_json::json!({"entries": requests.iter().map(|entry| format!("{entry:08x}")).collect::<Vec<_>>(), "exported_entries": entries.iter().map(|entry| format!("{entry:08x}")).collect::<Vec<_>>(), "recovery": recovery, "image_sha256": settings.sha256, "image_base": format!("{:08x}", image.base), "source": "original executable; live bytes not validated", "instruction_limit": if instruction_limit == usize::MAX { 0 } else { instruction_limit }, "instruction_count": discovered.instruction_lengths.len(), "eligibility": {"eligible": eligible, "reasons": reasons}, "switch_table_count": program.tables.len(), "recovery_limit_reached": limit_reached, "ranges": ranges, "discovery": discovered.report, "emitted": stats.emitted, "trapping": stats.trapping, "rejections": stats.rejections});
    std::fs::write(dir.join("jit.json"), serde_json::to_vec_pretty(&manifest)?)?;
    eprintln!(
        "[recomp] jit: batch {batch}: {} requests, {} bodies, {} traps",
        requests.len(),
        stats.emitted,
        stats.trapping
    );
    Ok((entries, new_ranges, limit_reached))
}
