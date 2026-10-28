import { platform } from "./platform.js";

const backend = platform();
const element = (id) => document.getElementById(id);
if (!navigator.platform.includes("Mac")) element("choose-source").textContent = "Choose game file";
const descriptions = {
  "ghost-recon": "The 2001 tactical shooter that started a franchise: small squads, careful planning, and unforgiving firefights.",
  "black-and-white": "Lionhead’s 2001 god game, where miracles and a creature that learned from you made kindness and cruelty part of play.",
  "railroad-tycoon-3": "The 2003 sequel that brought the railway empire builder into 3D, with fortunes riding on freight and the stock market.",
  "4x4-evo2": "Terminal Reality’s 2001 off-road racer, pairing trucks and SUVs with open terrain, career racing, and vehicle upgrades.",
  "civ4-bts": "The 2007 Civilization IV expansion that deepened empire building with espionage, corporations, and a wealth of scenarios.",
  "spore": "Maxis’s ambitious 2008 life simulator, taking player-made creatures from a single cell to a spacefaring civilization.",
};
const cardActions = { ready: "View installed game →", notInstalled: "Choose game files →", needsBuild: "Continue setup →", busy: "View setup →", unavailable: "Not available yet" };
const gigabytes = (bytes) => `${(bytes / 1e9).toFixed(1).replace(".", ",")} GB`;
let currentScreen = null;
let hasInspection = false;
let busy = false;
let checking = false;
let checksReady = false;
let selected = null;
let source = "";
let destination = "";
let lines = [];
let lastAction = "";
let checkRevision = 0;
let candidateId = "";
let inspectionReady = false;
let candidateCanInstall = false;
let resumeJob = null;
let sourceKind = "";
let candidateSummary = "";
let candidateContentRoot = "";
let candidateExecutableBytes = 0;
let preflightReady = false;
let preflightRevision = 0;
let activeJobId = "";
let failedJobId = "";
let inspecting = false;
let inspectionRevision = 0;

function message(text) {
  const target = element(element("installer").open ? "install-message" : "message");
  target.textContent = text;
  target.hidden = !text;
}

function errorText(error) {
  if (typeof error === "string") {
    try { return errorText(JSON.parse(error)); } catch { return error; }
  }
  if (error && typeof error === "object") {
    const diagnostic = error.diagnostic;
    const result = diagnostic?.message || error.message;
    if (typeof result === "string") {
      try { return errorText(JSON.parse(result)); } catch { return result; }
    }
    return result || JSON.stringify(error);
  }
  return String(error);
}

async function attempt(action) {
  try { await action(); } catch (error) { message(errorText(error)); }
}

function lock(value) {
  busy = value;
  for (const button of document.querySelectorAll("button")) {
    button.disabled = busy || checking || inspecting || button.dataset.unavailable === "true";
  }
  for (const input of document.querySelectorAll(".candidate-choice input")) {
    input.disabled = busy || checking || inspecting || input.dataset.unavailable === "true";
  }
  element("cancel").disabled = !busy;
  element("inspection-back").disabled = busy || checking || inspecting;
  element("inspection-cancel").hidden = !inspecting;
  element("inspection-cancel").disabled = !inspecting;
  element("review-copy").disabled = busy || checking || inspecting || !inspectionReady || !candidateCanInstall || !candidateId;
  element("install").disabled = busy || checking || inspecting || !checksReady || !preflightReady || !inspectionReady || !candidateCanInstall || !candidateId || !source || !destination;
  element("pick-destination").disabled = busy || checking || inspecting;
}

function renderRecovery() {
  const jobs = selected?.recoverableJobs || [];
  const panel = element("recovery");
  const list = element("recovery-jobs");
  list.replaceChildren();
  panel.hidden = busy || ![1, 3].includes(currentScreen) || !jobs.length;
  for (const job of jobs) {
    const row = document.createElement("article");
    row.className = "recovery-job";
    const heading = document.createElement("strong");
    heading.textContent = "Installation interrupted";
    const status = document.createElement("p");
    const latestError = job.error || job.errors?.at(-1);
    row.classList.toggle("current-failure", Boolean(latestError));
    status.textContent = latestError?.message || (job.canResume
      ? "Continue from the saved files."
      : "This installation needs attention before it can continue.");
    const destinationPath = document.createElement("p");
    destinationPath.textContent = job.destination || "";
    row.append(heading, status, destinationPath);
    if (latestError) {
      const details = document.createElement("details");
      const summary = document.createElement("summary");
      summary.textContent = "Error details";
      const technical = document.createElement("pre");
      technical.textContent = `${latestError.code || "Error"} · ${latestError.stage || job.stage}\n${latestError.path ? `Path: ${latestError.path}\n` : ""}${JSON.stringify(latestError.details || {}, null, 2)}`;
      details.append(summary, technical);
      row.append(details);
    }
    const actions = document.createElement("div");
    actions.className = "buttons recovery-actions";
    const more = document.createElement("details");
    const moreLabel = document.createElement("summary");
    moreLabel.textContent = "More options";
    const secondary = document.createElement("div");
    secondary.className = "buttons recovery-actions";
    const savedSource = document.createElement("p");
    savedSource.textContent = `Source: ${job.source?.path || "Not selected"}`;
    more.append(moreLabel, savedSource, secondary);
    const available = new Set(job.actions || []);
    const choices = [
      ["retry_failed_stage", "Try again", () => retrySavedJob(job)],
      ["locate_missing_file", "Locate game files", () => locateSource(job)],
      ["choose_another_copy", "Choose other files", chooseAnotherCopy],
      ["change_destination", "Change install location", () => changeDestination(job)],
      ["open_log", "View log", () => openJobLog(job)],
    ];
    let hasPrimary = false;
    for (const [action, label, handler] of choices) {
      if (!available.has(action)) continue;
      const button = document.createElement("button");
      button.textContent = label;
      button.onclick = () => attempt(handler);
      if (!hasPrimary) {
        button.className = "primary";
        actions.append(button);
        hasPrimary = true;
      } else secondary.append(button);
    }
    if (secondary.childElementCount) row.append(more);
    row.append(actions);
    list.append(row);
  }
}

function renderInspection(result) {
  hasInspection = true;
  const target = element("inspection");
  target.replaceChildren();
  const candidates = result?.candidates || [];
  inspectionReady = Boolean(result?.scanComplete && candidates.length);
  const chosen = candidates.length === 1 ? candidates[0] : null;
  candidateId = chosen?.id || "";
  candidateCanInstall = Boolean(chosen?.installable);
  candidateContentRoot = chosen?.contentRoot || "";
  candidateExecutableBytes = chosen?.executable?.size || 0;
  candidateSummary = chosen ? `${chosen.version?.label || chosen.status || "Copy selected"}${chosen.version?.preparations?.length ? ` · preparation: ${chosen.version.preparations.join(", ")}` : ""}` : "";
  if (candidates.length > 1) { inspectionReady = false; candidateCanInstall = false; }
  const updateResult = (candidate) => {
    const ready = Boolean(result?.scanComplete && candidate?.installable);
    const media = ["inno", "installshield", "unsupported_inno", "iso9660", "cabinet", "installshield_cab", "cue", "raw_cd", "media_hint", "installer_payload"].includes(sourceKind);
    let title = "This copy isn’t ready to install";
    let help = `Choose a complete game folder containing ${selected.executable}, or prepare the required files using a checklist.`;
    if (ready) {
      title = "Your copy is supported";
      help = ["inno", "installshield"].includes(sourceKind)
        ? "Executable version verified. Game data will be extracted and checked during installation."
        : "Version verified. Next, review the install location and build tools.";
    } else if (candidates.length > 1 && !candidate) {
      title = "Choose a copy to use";
      help = "Multiple copies found. Select one to check.";
    } else if (["inno", "installshield"].includes(sourceKind)) {
      title = "This installer needs attention";
      help = result.diagnostics?.[0]?.message || `The extracted files do not match the supported version of ${selected.title || selected.id}.`;
    } else if (media) {
      title = sourceKind.includes("inno") ? "This installer can’t be imported yet" : "This media can’t be imported yet";
      help = result.diagnostics?.[0]?.message || `Extract it, then choose the folder containing ${selected.executable}.`;
    } else if (result.diagnostics?.some((item) => item.code === "installer_selection_required")) {
      title = "Choose an installer";
      help = "Multiple installers found. Select one below to check.";
    } else if (!candidates.length && sourceKind === "folder") {
      title = "No game found here";
      help = `${result.scanComplete ? "" : "The search stopped before all files were checked. "}Choose the installed game folder containing ${selected.executable}, or select that executable directly.`;
    } else if (!result?.scanComplete) {
      title = "These files couldn’t be fully checked";
      help = `Choose ${selected.executable} directly, or a folder containing just the installed game.`;
    } else if (!candidates.length) {
      title = "This file isn’t a supported game";
      help = `Choose the installed game folder, ${selected.executable}, or a ZIP of the game files.`;
    }
    element("copy-result").classList.toggle("inspection-failed", !ready && !(candidates.length > 1 && !candidate));
    element("copy-result-title").textContent = title;
    element("copy-result-help").textContent = help;
    element("review-copy").hidden = !ready;
    element("choose-another").classList.toggle("primary", !ready);
    element("choose-another").textContent = media ? "Choose extracted game files" : "Choose game files";
    const primary = ready ? "review-copy" : "choose-another";
    element("copy-result").querySelector(".actions").append(element(primary));
  };
  updateResult(chosen);
  const renderPreparation = (candidate) => {
    const guidance = target.querySelector("#preparation-guidance");
    guidance.replaceChildren();
    const preparations = candidate?.version?.preparationDetails || [];
    for (const preparation of preparations) {
      const section = document.createElement("section");
      const heading = document.createElement("strong");
      heading.textContent = `Preparation required: ${preparation.id}`;
      const explanation = document.createElement("p");
      explanation.textContent = preparation.explanation || "Prepare the listed files manually, then inspect the result again.";
      section.append(heading, explanation);
      for (const [label, items] of [["Required inputs", preparation.requiredInputs], ["Tools", preparation.requiredTools], ["Expected result", preparation.outputs]]) {
        if (!items?.length) continue;
        const row = document.createElement("p");
        row.textContent = `${label}: ${items.join(", ")}`;
        section.append(row);
      }
      guidance.append(section);
    }
  };
  for (const candidate of candidates) {
    const label = document.createElement("label");
    label.className = "candidate-choice";
    const input = document.createElement("input");
    input.type = "radio";
    input.name = "candidate";
    input.value = candidate.id;
    input.dataset.unavailable = String(!result.scanComplete);
    input.disabled = !result.scanComplete || busy || checking;
    input.checked = candidate.id === candidateId;
    input.onchange = () => {
      candidateId = candidate.id;
      candidateContentRoot = candidate.contentRoot || "";
      candidateExecutableBytes = candidate.executable?.size || 0;
      candidateSummary = `${candidate.version?.label || candidate.status || "Copy selected"}${candidate.version?.preparations?.length ? ` · preparation: ${candidate.version.preparations.join(", ")}` : ""}`;
      if (resumeJob && candidate.id !== resumeJob.candidateId) resumeJob = null;
      inspectionReady = true;
      candidateCanInstall = Boolean(candidate.installable);
      updateResult(candidate);
      renderPreparation(candidate);
      preflightReady = false;
      if (!element("requirements").hidden) void updateSpaceEstimate();
      lock(false);
    };
    const detail = document.createElement("span");
    const relativeRoot = candidate.contentRootRelative && candidate.contentRootRelative !== "."
      ? ` · files in ${candidate.contentRootRelative}` : "";
    const candidateIssue = (candidate.diagnostics || []).find((item) => item.severity === "error");
    detail.textContent = `${candidate.version?.label || (candidate.installable ? "Supported version" : "Files need preparation")}${candidates.length > 1 ? ` · ${candidate.executable?.path || candidate.contentRootRelative || "Game copy"}` : relativeRoot}${candidateIssue ? ` · ${candidateIssue.message}` : ""}`;
    if (candidates.length > 1) label.append(input);
    label.append(detail);
    target.append(label);
  }
  const guidance = document.createElement("div");
  guidance.id = "preparation-guidance";
  target.append(guidance);
  renderPreparation(chosen);
  if (!candidates.length) {
    for (const diagnostic of result?.diagnostics || []) {
      for (const path of diagnostic.details?.installers || []) {
        const button = document.createElement("button");
        button.textContent = path;
        button.onclick = () => attempt(() => inspectSource(path));
        guidance.append(button);
      }
      for (const instruction of diagnostic.recovery || []) {
        const note = document.createElement("p");
        note.textContent = instruction;
        guidance.append(note);
      }
      for (const tool of diagnostic.details?.suggestedTools || []) {
        const row = document.createElement("p");
        const link = document.createElement("a");
        link.textContent = `Open ${tool.label} ↗`;
        link.href = tool.url;
        link.onclick = (event) => { event.preventDefault(); attempt(() => backend.open(tool.url)); };
        row.append(link);
        guidance.append(row);
      }
    }
  }
  const diagnostics = result?.diagnostics || [];
  if (diagnostics.length) {
    const details = document.createElement("details");
    const summary = document.createElement("summary");
    summary.textContent = "Inspection details";
    details.append(summary);
    for (const diagnostic of diagnostics) {
      const note = document.createElement("p");
      note.textContent = diagnostic.message;
      details.append(note);
    }
    target.append(details);
  }
  target.hidden = false;
  lock(false);
}

async function inspectSource(path) {
  hasInspection = false;
  const revision = ++inspectionRevision;
  const selectedGame = selected?.id;
  source = path;
  candidateId = "";
  preflightReady = false;
  preflightRevision++;
  inspectionReady = false;
  sourceKind = "";
  element("inspection").replaceChildren();
  element("inspection").hidden = true;
  element("selected-source").textContent = path;
  element("selected-source").hidden = false;
  element("copy-result").classList.remove("inspection-failed");
  element("copy-result-title").textContent = "Checking selected files…";
  element("copy-result-help").textContent = "Identifying the files and verifying the game version.";
  element("review-copy").hidden = true;
  message("");
  inspecting = true;
  step(4);
  lock(false);
  try {
    const result = await backend.query({ action: "inspect", game: selected.id, source });
    if (revision !== inspectionRevision || selectedGame !== selected?.id || source !== path) return null;
    inspecting = false;
    source = result.source?.path || source;
    sourceKind = result.source?.kind || "";
    renderInspection(result);
    element("selected-source").textContent = `Selected: ${source}`;
    element("selected-source").hidden = false;
    if (inspectionReady && candidateCanInstall && candidateId) void attempt(reviewInstallation);
    else step(4);
    return result;
  } catch (error) {
    if (revision !== inspectionRevision || selectedGame !== selected?.id) return null;
    candidateId = "";
    inspectionReady = false;
    sourceKind = "";
    inspecting = false;
    step(4);
    element("copy-result").classList.add("inspection-failed");
    element("copy-result-title").textContent = "These files couldn’t be checked";
    element("copy-result-help").textContent = errorText(error);
    lock(false);
    throw error;
  } finally {
    if (revision === inspectionRevision) {
      inspecting = false;
      lock(false);
    }
  }
}

function step(index, installing = false) {
  currentScreen = index;
  for (const [i, name] of ["requirements", "source", "job", "ready", "copy-result"].entries()) element(name).hidden = i !== index;
  element("recovery").hidden = ![1, 3].includes(index) || busy || !selected?.recoverableJobs?.length;
  element("setup").hidden = true;
  element("steps").hidden = index === 3 || (index === 2 && ["play", "log"].includes(lastAction));
  const needsAttention = index === 4 && !inspecting;
  const timeline = [[1, "Choose copy"], ...(needsAttention ? [[4, "Check copy"]] : []), [0, "Review"], [2, "Install"]];
  const activeScreen = index === 4 && inspecting ? 1 : index;
  const active = timeline.findIndex(([screen]) => screen === activeScreen);
  element("steps").replaceChildren(...timeline.map(([screen, label], i) => {
    const item = document.createElement("li");
    item.textContent = label;
    item.classList.toggle("active", screen === activeScreen);
    item.classList.toggle("done", i < active);
    if (screen === activeScreen) item.setAttribute("aria-current", "step");
    return item;
  }));
  element("source-selection").hidden = !source;
  element("source").classList.toggle("has-copy", Boolean(source));
  element("source-selection-path").textContent = source;
  element("source-selection-status").textContent = hasInspection ? candidateSummary || "Copy checked. Continue to view the result." : "This copy still needs to be checked.";
  element("choose-source").classList.toggle("primary", !source);
  element("choose-source").textContent = source ? "Change copy…" : navigator.platform.includes("Mac") ? "Choose files or folder" : "Choose game file";
  element("source-forward").hidden = !source;
  element("source-forward").textContent = hasInspection ? candidateCanInstall && inspectionReady ? "Continue to review" : "View copy details" : "Check this copy";
  if (index === 3) {
    element("installed-location").textContent = `Successfully installed at ${destination}`;
    element("update-note").hidden = !selected?.updateAvailable;
    element("rebuild").hidden = !selected?.updateAvailable;
  }
  if (index === 1) {
    element(source ? "source-forward" : "choose-source").focus();
  } else if ([0, 4].includes(index)) {
    const heading = element(["requirements", "source", "job", "ready", "copy-result"][index]).querySelector("h3");
    heading.tabIndex = -1;
    heading.focus();
  }
}

async function refresh() {
  const catalog = await backend.query({ action: "catalog" });
  element("storage").textContent = `Library records: ${catalog.storage}`;
  element("games").replaceChildren();
  for (const game of catalog.games) {
    const card = element("card").content.firstElementChild.cloneNode(true);
    card.querySelector(".game-title").textContent = game.title;
    card.querySelector(".state").textContent = game.updateAvailable ? "Update available" : "Installed";
    card.querySelector(".state").hidden = game.state !== "ready";
    card.querySelector(".state").classList.toggle("ready", game.state === "ready");
    card.dataset.unavailable = String(game.state === "unavailable");
    card.querySelector(".detail").textContent = descriptions[game.id] || game.detail || "Build a native app from your own game files.";
    card.querySelector(".card-action").textContent = cardActions[game.state] || "View setup →";
    card.onclick = () => attempt(() => openGame(game));
    element("games").append(card);
  }
  if (selected) selected = catalog.games.find((game) => game.id === selected.id) || selected;
  renderRecovery();
  element("play-existing").hidden = selected?.state !== "ready" || element("job").hidden || element("retry").hidden;
  if (selected && !element("ready").hidden) step(3);
  lock(busy);
}

async function openGame(game) {
  hasInspection = false;
  selected = game;
  failedJobId = "";
  activeJobId = "";
  checksReady = false;
  source = game.managedSource || "";
  sourceKind = game.managedSource ? "folder" : "";
  candidateId = "";
  preflightReady = false;
  preflightRevision++;
  inspectionReady = false;
  resumeJob = null;
  destination = game.installation || "";
  element("destination").value = destination;
  element("pick-destination").disabled = false;
  element("install-title").textContent = game.title;
  element("supported-version").textContent = game.supportedVersion || "Exact version not documented";
  element("source-notes").textContent = game.sourceNotes || `Choose the game folder containing ${game.executable}.`;
  element("selected-source").hidden = !source;
  element("selected-source").textContent = source ? `Using your library copy: ${source}` : "";
  element("installer").showModal();
  renderRecovery();
  message(game.maintenanceError || "");
  if (game.state === "ready") { step(3); return; }
  step(1);
  if (game.state === "unavailable") {
    element("checks").replaceChildren();
    element("check-status").textContent = game.detail || "This game is not available in this version of Recomp Toolkit.";
    element("recheck").hidden = true;
    return;
  }
  element("recheck").hidden = false;
}

async function checkRequirements() {
  const revision = ++checkRevision;
  checking = true;
  checksReady = false;
  lock(false);
  element("checks").replaceChildren();
  element("check-status").textContent = "Checking your computer…";
  element("recheck").hidden = false;
  element("build-requirements").hidden = true;
  try {
    const result = await backend.query({ action: "check", needsRust: selected?.needsRust ?? true, needsInnoextract: sourceKind === "inno", needsUnshield: sourceKind === "installshield" });
    if (revision !== checkRevision) return;
    element("check-status").textContent = result.ready ? "All requirements are installed." : "Install the missing tools below, then choose Check again.";
    for (const check of result.checks) {
      const row = document.createElement("div");
      row.className = `check ${check.ok ? "passed" : "missing"}`;
      const title = document.createElement("strong");
      title.textContent = `${check.ok ? "✓" : "!"} ${check.label}`;
      row.append(title);
      if (!check.ok) {
        const options = document.createElement("div");
        options.className = "buttons dependency-options";
        options.setAttribute("role", "group");
        options.setAttribute("aria-label", `${check.label} setup options`);
        const content = document.createElement("div");
        const methods = check.methods || [{ label: "Manual", help: check.help || check.detail, command: check.command, url: check.url }];
        function showMethod(method) {
          content.replaceChildren();
          for (const [i, button] of [...options.children].entries()) button.setAttribute("aria-pressed", String(methods[i] === method));
          const help = document.createElement("p");
          help.textContent = method.help;
          content.append(help);
          if (method.command) {
            const command = document.createElement("pre");
            command.textContent = method.command;
            content.append(command);
          }
          if (method.url) {
            const link = document.createElement("a");
            link.href = method.url;
            link.textContent = method.label === "Download" ? "Open download page ↗" : "Open setup guide ↗";
            link.onclick = (event) => { event.preventDefault(); attempt(() => backend.open(method.url)); };
            content.append(link);
          }
        }
        for (const method of methods) {
          const button = document.createElement("button");
          button.textContent = method.label;
          button.onclick = () => showMethod(method);
          options.append(button);
        }
        row.append(options, content);
        showMethod(methods[0]);
      }
      element("checks").append(row);
    }
    checksReady = result.ready;
    element("build-requirements").hidden = checksReady;
    element("build-requirements").open = !checksReady;
  } catch (error) {
    element("build-requirements").hidden = false;
    element("build-requirements").open = true;
    throw error;
  } finally {
    if (revision === checkRevision) {
      checking = false;
      lock(false);
    }
  }
}

async function chooseDroppedPath(path) {
  resumeJob = null;
  await inspectSource(path);
}

async function chooseSource() {
  const game = selected?.id;
  const path = await backend.pick("copy");
  if (path && selected?.id === game) await chooseDroppedPath(path);
}

async function retrySavedJob(job) {
  if (!job.canResume || !job.source?.path || !job.candidateId || !job.destination) {
    throw new Error("This saved job does not have enough validated information to retry.");
  }
  sourceKind = job.source.kind;
  checking = true;
  lock(false);
  let checked;
  try {
    checked = await backend.query({ action: "check", needsRust: selected?.needsRust ?? true,
      needsInnoextract: sourceKind === "inno", needsUnshield: sourceKind === "installshield" });
  } finally {
    checking = false;
    lock(false);
  }
  if (!checked.ready) {
    resumeJob = job;
    step(0);
    await checkRequirements();
    message("Install the missing build tools, then retry the saved job.");
    return;
  }
  resumeJob = job;
  source = job.source.path;
  candidateId = job.candidateId;
  destination = job.destination;
  element("destination").value = destination;
  await start({ action: "install", game: selected.id, source, candidateId, destination, jobId: job.jobId });
}

async function locateSource(job) {
  const kind = job.source?.kind === "folder" ? "folder" : "source";
  const path = await backend.pick(kind);
  if (!path) return;
  destination = job.destination;
  element("destination").value = destination;
  const result = await inspectSource(path);
  if (!result) return;
  const sameCandidate = result.candidates?.some((item) => item.installable && item.id === job.candidateId);
  resumeJob = sameCandidate ? job : null;
  if (!sameCandidate) {
    const candidate = result.candidates?.find((item) => item.id === job.candidateId);
    const issue = candidate?.diagnostics?.find((item) => item.severity === "error");
    message(`This copy does not match the saved job${issue ? `: ${issue.message}` : ""}. It remains untouched; choose an empty destination to start a new job.`);
  } else {
    source = path;
    candidateId = job.candidateId;
    message("The selected copy matches this saved job. Retry will recheck its full identity.");
  }
}

function chooseAnotherCopy() {
  resumeJob = null;
  source = "";
  hasInspection = false;
  candidateId = "";
  inspectionReady = false;
  element("selected-source").hidden = true;
  element("inspection").hidden = true;
  message("Choosing a different copy starts a new job. Choose an empty destination; saved jobs and their staged files remain untouched.");
  step(1);
  lock(false);
}

async function changeDestination(job) {
  if (job.source?.path && !source) source = job.source.path;
  const folder = await backend.pick("folder");
  if (!folder) return;
  resumeJob = null;
  const separator = folder.includes("\\") ? "\\" : "/";
  destination = `${folder.replace(/[\\/]+$/, "")}${separator}${selected.id}`;
  element("destination").value = destination;
  preflightReady = false;
  preflightRevision++;
  message("This destination applies to a new job; the saved job remains unchanged.");
  if (!inspectionReady) {
    if (source) await inspectSource(source);
    else step(1);
    return;
  }
  await reviewInstallation();
}

async function openJobLog(job) {
  lastAction = "log";
  element("job").classList.remove("job-failed");
  const result = await backend.query({ action: "job_log", game: selected.id, jobId: job.jobId });
  lines = result.text.split("\n");
  element("log").textContent = `${result.truncated ? "Earlier log output omitted. Showing the latest 256 KiB.\n\n" : ""}${result.text}`;
  element("log").closest("details").open = true;
  element("job-back").hidden = false;
  element("job-duration").hidden = true;
  element("stage").textContent = "Installation log";
  element("stage-help").textContent = `Job ${job.jobId}`;
  element("progress").hidden = true;
  element("current").hidden = true;
  element("cancel").hidden = true;
  element("retry").hidden = true;
  element("play-existing").hidden = selected?.state !== "ready";
  step(2);
  element("log").closest("details").querySelector("summary").focus();
}

async function start(request) {
  lock(true);
  message("");
  lines = [];
  lastAction = request.action;
  activeJobId = request.jobId || "";
  step(2);
  element("job").classList.remove("job-failed");
  element("job-back").hidden = true;
  element("retry").textContent = "Try again";
  element("log").textContent = "Waiting for installation output…";
  element("stage").textContent = request.action === "play" ? "Launching your game…" : "Preparing installation…";
  element("stage-help").textContent = "";
  element("job-duration").hidden = request.action === "play";
  element("current").textContent = "";
  element("progress").removeAttribute("value");
  element("progress").hidden = false;
  element("current").hidden = false;
  element("cancel").hidden = request.action === "play";
  element("retry").hidden = true;
  element("play-existing").hidden = true;
  try { await backend.start(request); } catch (error) {
    lock(false);
    element("cancel").hidden = true;
    element("retry").hidden = false;
    element("play-existing").hidden = selected?.state !== "ready";
    element("job").classList.add("job-failed");
    element("job-back").hidden = false;
    element("job-duration").hidden = true;
    element("progress").hidden = true;
    element("retry").classList.toggle("primary", selected?.state !== "ready");
    element("stage").textContent = "Couldn’t start";
    element("stage-help").textContent = errorText(error);
  }
}

function appendLog(text) {
  lines.push(text);
  if (lines.length > 2000) lines.shift();
  element("log").textContent = lines.join("\n");
}

function jobEvent(event) {
  if (event.kind === "stage") {
    if (event.jobId) {
      activeJobId = event.jobId;
      if (event.destination) destination = event.destination;
    }
    step(2, ["installing", "cleaning"].includes(event.stage));
    element("stage").textContent = event.message;
    appendLog(event.message);
    element("stage-help").textContent = {
      checking: "Checking build tools.",
      extracting: "Unpacking your copy without running the original installer.",
      validating: "Checking that your game version matches the supported release.",
      packaging: "Assembling the finished app and verifying its files before installation.",
      fingerprinting: "Verifying all game files. Large copies can take a while.",
      copying: "Copying your game files. Your originals stay untouched.",
      importing: "Copying your game files into the library. Your originals stay untouched.",
      building: "Compiling the app. This may take a while; compiler output appears below.",
      installing: `Saving the finished app in ${destination}.`,
      publishing: `Saving the finished app in ${destination}.`,
      cleaning: "Removing generated source, compiler output and build caches.",
    }[event.stage] || "";
    element("current").textContent = "";
    element("progress").removeAttribute("value");
  } else if (event.kind === "progress") {
    if (Number.isFinite(event.fraction)) element("progress").value = event.fraction;
    else element("progress").removeAttribute("value");
    const bytes = Number.isFinite(event.bytes) ? `${gigabytes(event.bytes)}${event.totalBytes ? ` / ${gigabytes(event.totalBytes)}` : ""} · ` : "";
    element("current").textContent = `${bytes}${event.current || ""}`;
  } else if (event.kind === "log") {
    appendLog(event.text);
  } else if (event.kind === "finished") {
    lock(false);
    element("cancel").hidden = true;
    const failed = event.outcome.kind === "error";
    if (failed) {
      failedJobId = event.outcome.diagnostic?.details?.jobId || activeJobId;
      element("stage").textContent = lastAction === "play" ? "Couldn’t launch the game" : "Installation stopped";
      element("job").classList.add("job-failed");
      element("job-back").hidden = false;
      element("job-duration").hidden = true;
      element("progress").hidden = true;
      element("current").hidden = true;
      element("stage-help").textContent = errorText(event.outcome.diagnostic || event.outcome.message);
      element("retry").textContent = "Try again";
      element("retry").hidden = false;
      element("retry").classList.toggle("primary", selected?.state !== "ready");
      message("");
    } else {
      if (lastAction === "install") Object.assign(selected, {
        state: "ready", updateAvailable: false, installation: destination, managedSource: `${destination}/Game Data`,
      });
      step(3);
      if (lastAction === "play") element("installer").close();
    }
    attempt(async () => {
      await refresh();
      if (failedJobId) resumeJob = selected?.recoverableJobs?.find((job) => job.jobId === failedJobId) || null;
      renderRecovery();
    });
  }
}

function close() {
  if (busy || checking || inspecting) return;
  checkRevision++;
  preflightRevision++;
  element("installer").close();
}

element("installer").addEventListener("cancel", (event) => { event.preventDefault(); close(); });
element("close").onclick = close;
element("refresh").onclick = () => attempt(refresh);
element("report-issues").onclick = () => attempt(() => backend.open("https://github.com/wraitii/recomp-toolkit/issues"));
element("recheck").onclick = () => attempt(checkRequirements);
async function reviewInstallation() {
  step(0);
  await checkRequirements();
  if (currentScreen === 0) await updateSpaceEstimate();
}
element("inspection-cancel").onclick = () => attempt(() => backend.cancel());
element("review-copy").onclick = () => attempt(reviewInstallation);
async function updateSpaceEstimate() {
  const revision = ++preflightRevision;
  preflightReady = false;
  if (!destination) {
    element("space-summary").textContent = "Choose a location to check available space.";
    lock(false);
    return;
  }
  element("space-summary").textContent = "Checking available space…";
  lock(false);
  if (!["folder", "executable", "zip", "inno", "installshield"].includes(sourceKind)) {
    element("space-summary").textContent = "Choose an installed folder, executable, ZIP or supported installer to estimate space.";
    lock(false);
    return;
  }
  try {
    const preflightSource = ["zip", "inno", "installshield"].includes(sourceKind) ? source : candidateContentRoot || source;
    const estimate = await backend.query({ action: "preflight", game: selected.id, source: preflightSource, sourceKind, destination });
    if (revision !== preflightRevision || currentScreen !== 0) return;
    const installedBytes = estimate.source.bytes + 40_000_000 + 10 * candidateExecutableBytes;
    element("space-summary").textContent = `About ${gigabytes(installedBytes)} installed · ${gigabytes(estimate.destination.requiredBytes)} free needed during the build.${estimate.destination.enough ? "" : ` Only ${gigabytes(estimate.destination.freeBytes)} available. Choose another location.`}`;
    preflightReady = estimate.destination.enough;
  } catch (error) {
    if (revision === preflightRevision) element("space-summary").textContent = `Space estimate failed: ${errorText(error)}`;
  }
  if (revision === preflightRevision) lock(false);
}
element("open-installation").onclick = () => attempt(() => backend.openFolder(destination));
element("rebuild").onclick = () => attempt(async () => {
  resumeJob = null;
  destination = selected.installation;
  element("destination").value = destination;
  await inspectSource(selected.managedSource);
});
element("choose-source").onclick = () => attempt(chooseSource);
element("choose-another").onclick = () => attempt(chooseSource);
element("source-forward").onclick = () => attempt(() => hasInspection ? candidateCanInstall && inspectionReady ? reviewInstallation() : step(4) : inspectSource(source));
element("inspection-back").onclick = () => {
  if (inspecting) {
    inspectionRevision++;
    inspecting = false;
  }
  message("");
  step(1);
  lock(false);
};
element("review-back").onclick = () => { message(""); step(1); };
element("install").onclick = () => attempt(() => {
  const request = { action: "install", game: selected.id, source, candidateId, destination };
  if (resumeJob && source === resumeJob.source?.path && destination === resumeJob.destination && candidateId === resumeJob.candidateId) request.jobId = resumeJob.jobId;
  else resumeJob = null;
  return start(request);
});
element("pick-destination").onclick = () => attempt(async () => {
  const folder = await backend.pick("folder");
  if (!folder) return;
  const separator = folder.includes("\\") ? "\\" : "/";
  destination = `${folder.replace(/[\\/]+$/, "")}${separator}${selected.id}`;
  element("destination").value = destination;
  preflightReady = false;
  if (!element("requirements").hidden) await updateSpaceEstimate();
  else element("space-summary").textContent = "Destination changed. Review the copy again to check space.";
  resumeJob = null;
  lock(false);
});
for (const name of ["play", "play-existing"]) element(name).onclick = () => attempt(() => start({ action: "play", game: selected.id }));
element("retry").onclick = () => attempt(async () => {
  if (lastAction === "play") { message(""); step(3); return; }
  if (failedJobId) {
    const saved = selected?.recoverableJobs?.find((job) => job.jobId === failedJobId);
    if (saved?.canResume) return retrySavedJob(saved);
    if (saved) throw new Error("This saved job needs a different recovery action. Choose it in Saved installation before retrying.");
  }
  source = selected.managedSource || source;
  if (selected.managedSource) destination = selected.installation;
  element("destination").value = destination;
  resumeJob = null;
  if (source) await inspectSource(source);
});
element("job-back").onclick = () => { message(""); step(lastAction === "play" ? 3 : 1); };
element("cancel").onclick = () => attempt(async () => {
  await backend.cancel();
  element("cancel").disabled = true;
  element("stage").textContent = "Stopping installation…";
});
element("repository").onclick = (event) => {
  event.preventDefault();
  attempt(() => backend.open(element("repository").href));
};
element("settings").onclick = () => {
  selected = null;
  source = "";
  element("install-title").textContent = "Recomp Toolkit settings";
  element("installer").showModal();
  for (const name of ["steps", "requirements", "source", "job", "ready", "copy-result"]) element(name).hidden = true;
  element("setup").hidden = false;
  element("recovery").hidden = true;
  message("");
};
element("installer").addEventListener("close", () => { element("steps").hidden = false; });
element("save").onclick = () => attempt(async () => {
  await backend.save({ kit: element("kit").value.trim(), python: element("python").value.trim() });
  await refresh();
  close();
  message("Settings saved.");
});
for (const [button, field, kind] of [["pick-kit", "kit", "kit"], ["pick-python", "python", "python"]]) {
  element(button).onclick = () => attempt(async () => {
    const path = await backend.pick(kind);
    if (path) element(field).value = path;
  });
}

await attempt(async () => {
  await backend.listen(jobEvent);
  window.__TAURI__?.event.listen("tauri://drag-drop", ({ payload }) => {
    const path = payload?.paths?.[0];
    if (path && selected && element("installer").open && !busy && !checking && !inspecting) attempt(() => chooseDroppedPath(path));
  });
  const settings = await backend.settings();
  element("kit").value = settings.kit;
  element("python").value = settings.python;
  await refresh();
});
