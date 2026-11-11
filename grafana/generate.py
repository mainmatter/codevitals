"""Generate grafana/dashboards/tally.json from the metric views in src/metrics.sql.

Run from the repository root after changing a metric: python3 grafana/generate.py
The test `dashboard_matches_metrics` fails until the dashboard is regenerated.
Each panel's description is its view's comment: a category, then What, Read and
Trend paragraphs.
"""
import json
import re

sql = open("src/metrics.sql").read()
comments = {m.group(1): m.group(2).replace("''", "'")
            for m in re.finditer(r"comment on view (m_\w+) is '((?:[^']|'')*)';", sql)}
views = re.findall(r"create or replace view (m_\w+)", sql)
assert set(views) == set(comments), (views, comments.keys())

DS = {"type": "yesoreyeram-infinity-datasource", "uid": "tally-infinity"}
TITLES = {
    "m_size": "Production code size",
    "m_hidden_coupling": "Commits crossing modules (vs chance)",
    "m_rework": "Rework within 7 days",
    "m_change_entropy": "Work spread: effective files",
    "m_module_entropy": "Work spread: effective modules",
    "m_ownership": "File ownership by top author",
    "m_decisions": "Decision points per 100 tokens",
    "m_nesting": "Block nesting below function body",
    "m_global_state": "Global mutable state per 1,000 tokens",
    "m_exceptions": "Throws and panics per 1,000 tokens",
    "m_goto": "goto per 1,000 C/C++ tokens",
    "m_encapsulation": "Encapsulated code share",
    "m_duplication": "Duplicated code share",
    "m_concentration": "Code in large functions",
    "m_dependency_reach": "Share of its language's code a file depends on",
    "m_cross_module_imports": "Imports crossing modules (vs chance)",
    "m_changed_file_reach": "How changed files rank by dependencies",
    "m_import_coverage": "Imports resolved (coverage of the charts beside)",
}
UNITS = {"m_size": "short", "m_hidden_coupling": "none", "m_change_entropy": "none",
         "m_module_entropy": "none", "m_decisions": "none", "m_nesting": "none",
         "m_global_state": "none", "m_exceptions": "none", "m_goto": "none",
         "m_cross_module_imports": "none"}  # others are shares in [0, 1]
LIFTS = {"m_hidden_coupling", "m_cross_module_imports"}  # 1 = chance; drawn as a dashed reference line
# Drawn as one line per language family instead of the combined value, which moves
# with the language mix.
FAMILIES = {"m_dependency_reach", "m_cross_module_imports", "m_import_coverage"}
FAMILY_LABELS = {"c": "C/C++", "rust": "Rust", "python": "Python", "ecma": "JS/TS"}
# Categorical series colours for Grafana's dark theme, in fixed order: blue, orange,
# aqua, yellow (dataviz reference palette, dark steps). Checked against the dark
# panel surface #181b1f for colour-vision-deficiency separation and contrast.
PALETTE = ["#3987e5", "#d95926", "#199e70", "#c98500"]
SECOND_AXIS = {"m_size": ("files", "files"),  # an extra series on the right axis
               "m_nesting": ("deep_share", "share nested deeper than 3"),
               "m_change_entropy": ("files_per_commit", "files per commit"),
               "m_module_entropy": ("modules", "modules"),
               "m_changed_file_reach": ("reach", "files they depend on")}
LOG_SCALE = {"m_change_entropy", "m_module_entropy"}
BANDS = {"m_hidden_coupling", "m_rework", "m_ownership", "m_changed_file_reach"}  # low/high columns: two standard errors
ROWS = {"SIZE": "Size", "CHANGES": "How changes spread", "ENTROPY": "Entropy of change",
        "STRUCTURE": "Code structure", "COMPLEXITY": "Local complexity",
        "NONLOCAL": "Non-local control flow and state", "DEPENDENCIES": "Dependencies through imports"}
GUIDE = """**How to read this dashboard.** Charts count production code only, not tests, vendored or non-code files. Hover a title for what a chart measures and how to read it.
Change and entropy charts use the last 100 commits that touched code, read weekly; neighbouring points share most commits, and faint lines span two standard errors. A dashed line at 1 marks chance: module boundaries make no difference.
Dependency charts draw one line per language. A file's *reach* is the files it depends on through its imports and theirs.
No metric is validated against defects yet: read trends within this repository, not absolute values."""


def arrow(view):
    text = comments[view]
    return " ↑" if "Higher is better" in text else " ↓" if "Lower is better" in text else ""


def target(ref, view, columns):
    return {"refId": ref, "datasource": DS, "type": "json", "source": "url", "url": "${metrics_url}",
            "url_options": {"method": "GET", "data": ""}, "parser": "backend",
            "root_selector": f"$.{view}", "format": "timeseries",
            "columns": [{"selector": "time", "text": "time", "type": "timestamp"}] + columns}


def panel(pid, view, x, y, w):
    columns = [{"selector": "value", "text": TITLES[view], "type": "number"}]
    style = {}  # series label -> extra override properties
    if view in FAMILIES:
        columns = [{"selector": f, "text": label, "type": "number"} for f, label in FAMILY_LABELS.items()]
    if view in BANDS:
        for column, label in (("low", "-2 standard errors"), ("high", "+2 standard errors")):
            columns.append({"selector": column, "text": label, "type": "number"})
            style[label] = [{"id": "custom.lineStyle", "value": {"fill": "dash", "dash": [4, 4]}}]
    if view in SECOND_AXIS:
        column, label = SECOND_AXIS[view]
        columns.append({"selector": column, "text": label, "type": "number"})
        style[label] = [{"id": "custom.axisPlacement", "value": "right"}, {"id": "unit", "value": "short"}]
    # Every series in a panel gets its own palette slot, in fixed order; a language
    # family keeps its slot in every panel.
    assert len(columns) <= len(PALETTE), (view, len(columns))
    overrides = [{"matcher": {"id": "byName", "options": c["text"]},
                  "properties": [{"id": "color", "value": {"mode": "fixed", "fixedColor": color}}]
                  + style.get(c["text"], [])}
                 for c, color in zip(columns, PALETTE)]
    targets = [target("A", view, columns)]
    defaults = {"unit": UNITS.get(view, "percentunit"), "decimals": 2,
                "custom": {"drawStyle": "line", "lineWidth": 2, "spanNulls": False,
                           "showPoints": "never", "fillOpacity": 0}}
    if view in LOG_SCALE:
        defaults["custom"]["scaleDistribution"] = {"type": "log", "log": 2}
    if view in LIFTS:
        defaults["thresholds"] = {"mode": "absolute",
                                  "steps": [{"color": "transparent", "value": None},
                                            {"color": "text", "value": 1}]}
        defaults["custom"]["thresholdsStyle"] = {"mode": "dashed"}
    return {"id": pid, "type": "timeseries", "title": TITLES[view] + arrow(view),
            "description": comments[view], "datasource": DS,
            "gridPos": {"h": 9, "w": w, "x": x, "y": y}, "targets": targets,
            "fieldConfig": {"defaults": defaults, "overrides": overrides},
            "options": {"legend": {"showLegend": len(columns) > 1, "displayMode": "list",
                                   "placement": "bottom"},
                        "tooltip": {"mode": "multi", "sort": "none"}}}


panels = [{"id": 1, "type": "text", "title": "", "gridPos": {"h": 4, "w": 24, "x": 0, "y": 0},
           "options": {"mode": "markdown", "content": GUIDE}}]
pid, y = 2, 4
groups = {}
for v in views:
    groups.setdefault(comments[v].split(".", 1)[0].strip(), []).append(v)
for category, vs in groups.items():
    panels.append({"id": pid, "type": "row", "title": ROWS[category], "collapsed": False,
                   "gridPos": {"h": 1, "w": 24, "x": 0, "y": y}, "panels": []})
    pid += 1
    y += 1
    w = 24 // min(len(vs), 3)
    for i, v in enumerate(vs):
        panels.append(panel(pid, v, (i % 3) * w, y + (i // 3) * 9, w))
        pid += 1
    y += 9 * ((len(vs) + 2) // 3)

dashboard = {
    "uid": "tally", "title": "tally", "tags": ["tally"], "editable": True, "schemaVersion": 39,
    "time": {"from": "now-15y", "to": "now"}, "timezone": "utc", "graphTooltip": 1,
    "templating": {"list": [{"name": "metrics_url", "label": "Metrics URL", "type": "textbox",
                             "query": "http://127.0.0.1:8123/metrics.json",
                             "current": {"text": "http://127.0.0.1:8123/metrics.json",
                                         "value": "http://127.0.0.1:8123/metrics.json"}}]},
    "panels": panels,
}
with open("grafana/dashboards/tally.json", "w") as out:
    json.dump(dashboard, out, indent=2, ensure_ascii=False)
    out.write("\n")
print(f"{len(views)} metric panels in rows {[ROWS[c] for c in groups]}")
