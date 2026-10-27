"""Everything that steers one translation, read once from game.toml [translate] and the command line."""
from pathlib import Path

from program import TranslateError
from ir.body import EmissionPolicy
from ir import analysis_facts


class Settings(object):
    def __init__(self, cfg, jobs=1, module=None):
        translate = cfg["translate"]
        if not cfg.get("code_map_path"):
            raise TranslateError("game.toml [translate] code_map is required")
        self.cfg = cfg
        self.curated = {**{"globals." + name: entry for name, entry in cfg["globals"].items()},
                        **cfg["curated"]}
        self.game_dir = Path(cfg["dir"])
        self.exe = cfg["developer_exe_path"]
        self.code_map = Path(cfg["code_map_path"])
        self.module = next((m for m in cfg["aux_modules"] if m["key"] == module), None)
        if module and self.module is None:
            raise TranslateError("game.toml has no [modules.aux.%s]" % module)
        self.base = cfg["game"]["image_base"]
        self.sha256 = cfg["game"]["sha256"]
        if self.module:
            self.exe = self.module["path"]
            self.code_map = self.module["code_map_path"]
            self.base = self.module["base"]
            self.sha256 = self.module["sha256"]
            self.curated = {}
        self.alternate_entries = frozenset(int(a) for a in translate["alternate_entries"])
        self.configured_entries = (frozenset(self.module["entry_points"]) if self.module else
                                   self.alternate_entries | frozenset(int(a) for a in translate.get("entry_points", ())))
        if self.module:
            self.alternate_entries = frozenset()
        native = translate.get("native")
        self.native_header = native.get("header") if isinstance(native, dict) else None
        if self.module:
            self.native_header = None
        metadata = translate.get("x87_mode_metadata")
        self.x87_mode_metadata = self.game_dir / metadata if metadata and not self.module else None
        facts = translate.get("x87_facts")
        self.x87_facts = self.game_dir / facts if facts and not self.module else None
        analysis = translate.get("analysis_facts")
        self.analysis_facts = self.game_dir / analysis if analysis and not self.module else None
        self.analysis_targets = None
        indirect = translate.get("indirect_targets")
        self.indirect_targets = self.game_dir / indirect if indirect and not self.module else None
        if self.analysis_facts and (self.indirect_targets is not None or translate.get("call_effects")):
            raise TranslateError("analysis_facts cannot be combined with indirect_targets or call_effects")
        self.allocators = (frozenset(row["function"] for row in translate.get("allocators", ()))
                           if not self.module else frozenset())
        self.call_effects = {"sites": {}, "imports": {}}
        if self.analysis_facts:
            loaded = analysis_facts.load(self.analysis_facts, self.sha256)
            self.analysis_targets = {site: (row["kind"], row["targets"])
                                     for site, row in loaded["sites"].items()
                                     if row["targets"] is not None}
            for site, row in loaded["sites"].items():
                if row["effect"] is not None:
                    if row["kind"] != "call":
                        raise TranslateError("%s: jump site %08x cannot carry a call effect"
                                             % (self.analysis_facts, site))
                    self.call_effects["sites"][site] = row["effect"]
            self.call_effects["imports"] = {name: row["effect"]
                                            for name, row in loaded["imports"].items()}
        else:
            for row in translate.get("call_effects", []):
                fields = {field: row[field] for field in ("regs", "x87") if field in row}
                if "site" in row:
                    self.call_effects["sites"][row["site"]] = fields
                else:
                    self.call_effects["imports"][row["import"]] = fields
        self.x87_imports_preserve_pc_rc = translate["x87_imports_preserve_pc_rc"]
        self.x87_entry_contracts = translate.get("x87_entry_contracts", []) if not self.module else []
        self.call_contracts = translate["call_contracts"]
        self.private_leaf_contracts = translate["private_leaf_contracts"] and not self.module
        self.private_input_arguments = translate["private_input_arguments"] and not self.module
        registers = translate.get("private_input_registers", {}) if not self.module else {}
        if any(len(names) != 1 or names[0] == "ST0" for names in registers.values()):
            raise ValueError("the Python translator passes one general private input register per function")
        self.private_input_registers = {addr: names[0] for addr, names in registers.items()}
        self.checked_returns = frozenset(translate["checked_returns"]) if not self.module else frozenset()
        self.state_contract_callers = frozenset(translate["state_contract_callers"]) if not self.module else frozenset()
        self.discover = translate.get("discover", True)
        self.discovered = None
        self.discovered_checked = False
        self.discovered_dropped = {}
        self.jobs = jobs
        relaxed = translate["fault_state"] == "relaxed"
        self.emit = EmissionPolicy(
            x87_scalar_strict=not relaxed, local_state=relaxed, lazy_flags=relaxed,
            msvc_convention=translate["msvc_x87_convention"],
            x87_cw_clone=translate["x87_cw_clone"], indirect_call_symbol="recomp_call")
