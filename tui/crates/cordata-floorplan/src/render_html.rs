use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use crate::model::Floorplan;

pub fn write_html_report(fp: &Floorplan, out_path: &Path) -> Result<()> {
    if let Some(parent) = out_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed creating directory {}", parent.display()))?;
    }

    let json_data = serde_json::to_string(fp).context("failed serializing floorplan to json")?;
    let html = generate_html(fp, &json_data);

    fs::write(out_path, html)
        .with_context(|| format!("failed writing html to {}", out_path.display()))?;
    Ok(())
}

fn generate_html(fp: &Floorplan, json_data: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>Cordata Floorplan — {project_name}</title>
<style>
  :root {{
    --bg: #090d16;
    --panel-bg: #101726;
    --panel-border: #1e2c48;
    --grid-line: rgba(30, 60, 110, 0.25);
    --text-main: #e2e8f0;
    --text-muted: #7e8c9f;
    --accent-blue: #38bdf8;
    --accent-cyan: #06b6d4;
    --accent-yellow: #fbbf24;
    --accent-green: #34d399;
    --accent-magenta: #c084fc;
    --font-mono: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
    --font-sans: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  }}

  * {{ box-sizing: border-box; margin: 0; padding: 0; }}

  body {{
    background-color: var(--bg);
    background-image: 
      linear-gradient(var(--grid-line) 1px, transparent 1px),
      linear-gradient(90deg, var(--grid-line) 1px, transparent 1px);
    background-size: 32px 32px;
    color: var(--text-main);
    font-family: var(--font-sans);
    display: flex;
    flex-direction: column;
    min-height: 100vh;
    overflow-x: hidden;
  }}

  /* Blueprint Header */
  header {{
    background: rgba(16, 23, 38, 0.85);
    backdrop-filter: blur(8px);
    border-bottom: 2px solid var(--panel-border);
    padding: 16px 24px;
    display: flex;
    justify-content: space-between;
    align-items: center;
    position: sticky;
    top: 0;
    z-index: 50;
  }}

  .header-brand {{
    display: flex;
    align-items: center;
    gap: 12px;
  }}

  .blueprint-icon {{
    width: 28px;
    height: 28px;
    background: linear-gradient(135deg, var(--accent-cyan), var(--accent-blue));
    border-radius: 6px;
    display: flex;
    align-items: center;
    justify-content: center;
    font-weight: bold;
    color: #05101f;
    font-family: var(--font-mono);
  }}

  .header-title h1 {{
    font-size: 1.15rem;
    font-weight: 700;
    letter-spacing: 0.05em;
    text-transform: uppercase;
    color: var(--accent-cyan);
  }}

  .header-title p {{
    font-size: 0.8rem;
    color: var(--text-muted);
    font-family: var(--font-mono);
  }}

  .stats-bar {{
    display: flex;
    gap: 16px;
    align-items: center;
  }}

  .stat-pill {{
    display: flex;
    align-items: center;
    gap: 6px;
    background: rgba(15, 23, 42, 0.6);
    border: 1px solid var(--panel-border);
    padding: 6px 12px;
    border-radius: 9999px;
    font-size: 0.8rem;
    font-family: var(--font-mono);
  }}

  .dot {{
    width: 8px;
    height: 8px;
    border-radius: 50%;
  }}
  .dot.active {{ background: var(--accent-cyan); box-shadow: 0 0 8px var(--accent-cyan); }}
  .dot.modified {{ background: var(--accent-yellow); box-shadow: 0 0 8px var(--accent-yellow); }}
  .dot.clean {{ background: var(--accent-green); }}

  /* Controls Bar */
  .controls-bar {{
    padding: 12px 24px;
    display: flex;
    justify-content: space-between;
    align-items: center;
    border-bottom: 1px solid var(--panel-border);
    background: rgba(10, 15, 25, 0.5);
  }}

  .filter-group {{
    display: flex;
    gap: 8px;
  }}

  .filter-btn {{
    background: transparent;
    border: 1px solid var(--panel-border);
    color: var(--text-muted);
    padding: 6px 14px;
    border-radius: 6px;
    cursor: pointer;
    font-size: 0.8rem;
    font-family: var(--font-mono);
    transition: all 0.2s;
  }}

  .filter-btn:hover, .filter-btn.active {{
    background: var(--panel-border);
    color: var(--text-main);
    border-color: var(--accent-blue);
  }}

  .search-input {{
    background: rgba(16, 23, 38, 0.8);
    border: 1px solid var(--panel-border);
    color: var(--text-main);
    padding: 6px 12px;
    border-radius: 6px;
    font-size: 0.85rem;
    font-family: var(--font-mono);
    width: 260px;
  }}
  .search-input:focus {{
    outline: none;
    border-color: var(--accent-cyan);
  }}

  /* Main Architectural Canvas */
  main {{
    flex: 1;
    padding: 24px;
    display: flex;
    flex-direction: column;
    gap: 32px;
    max-width: 1400px;
    margin: 0 auto;
    width: 100%;
  }}

  .layer-section {{
    background: rgba(16, 23, 38, 0.4);
    border: 1px dashed var(--panel-border);
    border-radius: 12px;
    padding: 20px;
    position: relative;
  }}

  .layer-label {{
    position: absolute;
    top: -10px;
    left: 20px;
    background: var(--bg);
    padding: 2px 10px;
    font-size: 0.75rem;
    font-family: var(--font-mono);
    text-transform: uppercase;
    letter-spacing: 0.1em;
    color: var(--accent-blue);
    border: 1px solid var(--panel-border);
    border-radius: 4px;
  }}

  .rooms-grid {{
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
    gap: 18px;
    margin-top: 8px;
  }}

  /* Architectural Room Card */
  .room-card {{
    background: var(--panel-bg);
    border: 1px solid var(--panel-border);
    border-radius: 8px;
    padding: 16px;
    cursor: pointer;
    transition: transform 0.2s, border-color 0.2s, box-shadow 0.2s;
    position: relative;
    display: flex;
    flex-direction: column;
    gap: 10px;
  }}

  .room-card:hover {{
    transform: translateY(-2px);
    border-color: var(--accent-blue);
  }}

  .room-card.status-active {{
    border-color: var(--accent-cyan);
    box-shadow: 0 0 16px rgba(6, 182, 212, 0.35);
    animation: pulse-glow 2.5s infinite;
  }}

  @keyframes pulse-glow {{
    0%, 100% {{ box-shadow: 0 0 12px rgba(6, 182, 212, 0.3); }}
    50% {{ box-shadow: 0 0 24px rgba(6, 182, 212, 0.6); }}
  }}

  .room-card.status-modified {{
    border-color: var(--accent-yellow);
    box-shadow: 0 0 12px rgba(251, 191, 36, 0.25);
  }}

  .room-header {{
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
  }}

  .room-title {{
    font-size: 1rem;
    font-weight: 600;
    color: var(--text-main);
    display: flex;
    align-items: center;
    gap: 8px;
  }}

  .role-tag {{
    font-size: 0.65rem;
    font-family: var(--font-mono);
    padding: 2px 6px;
    border-radius: 4px;
    background: rgba(192, 132, 252, 0.15);
    color: var(--accent-magenta);
    border: 1px solid rgba(192, 132, 252, 0.3);
  }}

  .status-badge {{
    font-size: 0.7rem;
    font-family: var(--font-mono);
    padding: 3px 8px;
    border-radius: 12px;
    display: flex;
    align-items: center;
    gap: 4px;
  }}
  .badge-active {{ background: rgba(6, 182, 212, 0.15); color: var(--accent-cyan); border: 1px solid var(--accent-cyan); }}
  .badge-modified {{ background: rgba(251, 191, 36, 0.15); color: var(--accent-yellow); border: 1px solid var(--accent-yellow); }}
  .badge-clean {{ background: rgba(52, 211, 153, 0.1); color: var(--accent-green); border: 1px solid rgba(52, 211, 153, 0.2); }}

  .room-summary {{
    font-size: 0.82rem;
    color: var(--text-muted);
    line-height: 1.35;
  }}

  .room-path {{
    font-size: 0.72rem;
    font-family: var(--font-mono);
    color: #64748b;
  }}

  .room-footer {{
    display: flex;
    justify-content: space-between;
    align-items: center;
    font-size: 0.75rem;
    font-family: var(--font-mono);
    color: var(--text-muted);
    border-top: 1px solid rgba(30, 44, 72, 0.6);
    padding-top: 8px;
    margin-top: auto;
  }}

  .dep-tags {{
    display: flex;
    gap: 4px;
    flex-wrap: wrap;
    max-width: 200px;
  }}

  .dep-tag {{
    background: rgba(30, 44, 72, 0.5);
    padding: 1px 5px;
    border-radius: 3px;
    font-size: 0.65rem;
  }}

  /* Drawer / Modal Inspector */
  .drawer-overlay {{
    position: fixed;
    top: 0; left: 0; right: 0; bottom: 0;
    background: rgba(0, 0, 0, 0.6);
    display: none;
    z-index: 100;
  }}
  .drawer-overlay.open {{ display: block; }}

  .drawer {{
    position: fixed;
    top: 0; right: 0; bottom: 0;
    width: 480px;
    max-width: 90vw;
    background: #0d1320;
    border-left: 2px solid var(--panel-border);
    padding: 24px;
    display: flex;
    flex-direction: column;
    gap: 16px;
    z-index: 101;
    transform: translateX(100%);
    transition: transform 0.3s ease;
  }}
  .drawer.open {{ transform: translateX(0); }}

  .drawer-close {{
    align-self: flex-end;
    background: transparent;
    border: none;
    color: var(--text-muted);
    font-size: 1.2rem;
    cursor: pointer;
  }}
  .drawer-close:hover {{ color: var(--text-main); }}

  .files-list {{
    overflow-y: auto;
    flex: 1;
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-family: var(--font-mono);
    font-size: 0.78rem;
  }}

  .file-item {{
    padding: 6px 10px;
    border-radius: 4px;
    background: rgba(16, 23, 38, 0.6);
    display: flex;
    justify-content: space-between;
    align-items: center;
  }}
  .file-item.mod {{ border-left: 3px solid var(--accent-yellow); }}
  .file-item.act {{ border-left: 3px solid var(--accent-cyan); }}

  /* Flow Wire Arrows */
  .layer-connector {{
    display: flex;
    justify-content: center;
    align-items: center;
    gap: 12px;
    padding: 4px 0;
    color: var(--accent-cyan);
    font-family: var(--font-mono);
    font-size: 0.8rem;
  }}
</style>
</head>
<body>

<header>
  <div class="header-brand">
    <div class="blueprint-icon">FP</div>
    <div class="header-title">
      <h1>{project_name} — Architectural Floorplan</h1>
      <p>{root}</p>
    </div>
  </div>
  <div class="stats-bar">
    <div class="stat-pill"><div class="dot active"></div> Active: <strong>{active_count}</strong></div>
    <div class="stat-pill"><div class="dot modified"></div> Modified: <strong>{modified_count}</strong></div>
    <div class="stat-pill"><div class="dot clean"></div> Clean: <strong>{clean_count}</strong></div>
    <div class="stat-pill">Components: <strong>{total_components}</strong></div>
    <div class="stat-pill">Total LOC: <strong>{total_lines}</strong></div>
  </div>
</header>

<div class="controls-bar">
  <div class="filter-group">
    <button class="filter-btn active" data-filter="all">All Rooms</button>
    <button class="filter-btn" data-filter="active">Active Only</button>
    <button class="filter-btn" data-filter="modified">Modified Only</button>
    <button class="filter-btn" data-filter="clean">Clean Only</button>
  </div>
  <input type="text" class="search-input" id="search" placeholder="Search components or files...">
</div>

<main id="rooms-container">
  <!-- Dynamically generated from json_data -->
</main>

<div class="drawer-overlay" id="overlay" onclick="closeDrawer()"></div>
<div class="drawer" id="drawer">
  <button class="drawer-close" onclick="closeDrawer()">&times;</button>
  <div id="drawer-content"></div>
</div>

<script>
  const FLOORPLAN = {json_data};

  function renderFloorplan(filter = 'all', searchQuery = '') {{
    const container = document.getElementById('rooms-container');
    container.innerHTML = '';

    // Group by layer
    const layerMap = new Map();
    FLOORPLAN.components.forEach(comp => {{
      // Filter logic
      const statusType = comp.status.type;
      if (filter === 'active' && statusType !== 'active') return;
      if (filter === 'modified' && statusType !== 'modified') return;
      if (filter === 'clean' && statusType !== 'clean') return;

      if (searchQuery) {{
        const q = searchQuery.toLowerCase();
        const matchesName = comp.name.toLowerCase().includes(q);
        const matchesSummary = comp.summary.toLowerCase().includes(q);
        const matchesFile = comp.files.some(f => f.relative_path.toLowerCase().includes(q));
        if (!matchesName && !matchesSummary && !matchesFile) return;
      }}

      if (!layerMap.has(comp.layer)) layerMap.set(comp.layer, []);
      layerMap.get(comp.layer).push(comp);
    }});

    const sortedLayers = Array.from(layerMap.keys()).sort((a, b) => b - a); // Top UI layers first

    sortedLayers.forEach((layerIdx, idx) => {{
      const comps = layerMap.get(layerIdx);
      const section = document.createElement('div');
      section.className = 'layer-section';

      const layerTitle = getLayerTitle(layerIdx);
      section.innerHTML = `<div class="layer-label">${{layerTitle}} (Layer ${{layerIdx}})</div>`;

      const grid = document.createElement('div');
      grid.className = 'rooms-grid';

      comps.forEach(c => {{
        const card = createRoomCard(c);
        grid.appendChild(card);
      }});

      section.appendChild(grid);
      container.appendChild(section);

      if (idx < sortedLayers.length - 1) {{
        const connector = document.createElement('div');
        connector.className = 'layer-connector';
        connector.innerHTML = '▼ ▼ ▼ depends on lower layers ▼ ▼ ▼';
        container.appendChild(connector);
      }}
    }});
  }}

  function getLayerTitle(idx) {{
    switch(idx) {{
      case 0: return 'Foundation & Core Systems';
      case 1: return 'Execution & Orchestration Engines';
      case 2: return 'UI & Client Interfaces';
      case 3: return 'Developer & System Tools';
      default: return 'Subsystem Modules';
    }}
  }}

  function createRoomCard(comp) {{
    const card = document.createElement('div');
    const statusType = comp.status.type;
    card.className = `room-card status-${{statusType}}`;
    card.onclick = () => openDrawer(comp);

    let statusBadge = '';
    if (statusType === 'active') {{
      const count = comp.status.active_files ? comp.status.active_files.length : 1;
      statusBadge = `<span class="status-badge badge-active">★ Active (${{count}})</span>`;
    }} else if (statusType === 'modified') {{
      const count = comp.status.changed_files ? comp.status.changed_files.length : 1;
      statusBadge = `<span class="status-badge badge-modified">● Modified (${{count}})</span>`;
    }} else {{
      statusBadge = `<span class="status-badge badge-clean">✓ Clean</span>`;
    }}

    const depsHtml = comp.dependencies.slice(0, 3).map(d => `<span class="dep-tag">${{d}}</span>`).join('');

    card.innerHTML = `
      <div class="room-header">
        <div class="room-title">
          ${{comp.name}}
          <span class="role-tag">${{comp.role.toUpperCase()}}</span>
        </div>
        ${{statusBadge}}
      </div>
      <div class="room-summary">${{comp.summary}}</div>
      <div class="room-path">${{comp.path}}</div>
      <div class="room-footer">
        <div>${{comp.files.length}} files · ${{comp.total_lines.toLocaleString()}} lines</div>
        <div class="dep-tags">${{depsHtml}}</div>
      </div>
    `;
    return card;
  }}

  function openDrawer(comp) {{
    const overlay = document.getElementById('overlay');
    const drawer = document.getElementById('drawer');
    const content = document.getElementById('drawer-content');

    const statusType = comp.status.type;
    const statusText = statusType === 'active' ? 'Active Scope' : (statusType === 'modified' ? 'Uncommitted Changes' : 'Clean');

    let filesHtml = comp.files.map(f => {{
      let cls = '';
      let badge = '';
      if (f.is_active) {{ cls = 'act'; badge = '<span style="color:var(--accent-cyan)">★ active</span>'; }}
      else if (f.is_modified) {{ cls = 'mod'; badge = '<span style="color:var(--accent-yellow)">● mod</span>'; }}
      return `<div class="file-item ${{cls}}"><span>${{f.relative_path}}</span><span>${{badge}} ${{f.lines}} lines</span></div>`;
    }}).join('');

    content.innerHTML = `
      <h2 style="color:var(--accent-cyan); font-size:1.3rem; margin-bottom:4px;">${{comp.name}}</h2>
      <p style="color:var(--text-muted); font-size:0.85rem; font-family:var(--font-mono); margin-bottom:12px;">${{comp.path}}</p>
      <p style="font-size:0.9rem; margin-bottom:16px;">${{comp.summary}}</p>
      
      <div style="display:flex; gap:12px; margin-bottom:16px; font-family:var(--font-mono); font-size:0.8rem;">
        <div class="stat-pill">Status: <strong>${{statusText}}</strong></div>
        <div class="stat-pill">Files: <strong>${{comp.files.length}}</strong></div>
        <div class="stat-pill">Lines: <strong>${{comp.total_lines.toLocaleString()}}</strong></div>
      </div>

      <h4 style="font-size:0.85rem; text-transform:uppercase; color:var(--text-muted); margin-bottom:8px; font-family:var(--font-mono);">
        Dependencies (${{comp.dependencies.length}})
      </h4>
      <div style="display:flex; gap:6px; flex-wrap:wrap; margin-bottom:16px;">
        ${{comp.dependencies.length ? comp.dependencies.map(d => `<span class="stat-pill">${{d}}</span>`).join('') : '<span style="color:var(--text-muted); font-size:0.8rem;">None</span>'}}
      </div>

      <h4 style="font-size:0.85rem; text-transform:uppercase; color:var(--text-muted); margin-bottom:8px; font-family:var(--font-mono);">
        Source Files
      </h4>
      <div class="files-list">
        ${{filesHtml}}
      </div>
    `;

    overlay.classList.add('open');
    drawer.classList.add('open');
  }}

  function closeDrawer() {{
    document.getElementById('overlay').classList.remove('open');
    document.getElementById('drawer').classList.remove('open');
  }}

  // Filter click handlers
  document.querySelectorAll('.filter-btn').forEach(btn => {{
    btn.onclick = () => {{
      document.querySelectorAll('.filter-btn').forEach(b => b.classList.remove('active'));
      btn.classList.add('active');
      const filter = btn.dataset.filter;
      const search = document.getElementById('search').value;
      renderFloorplan(filter, search);
    }};
  }});

  document.getElementById('search').oninput = (e) => {{
    const activeFilter = document.querySelector('.filter-btn.active').dataset.filter;
    renderFloorplan(activeFilter, e.target.value);
  }};

  // Initial render
  renderFloorplan();
</script>
</body>
</html>
"#,
        project_name = fp.project_name,
        root = fp.root,
        active_count = fp.metrics.active_components,
        modified_count = fp.metrics.modified_components,
        clean_count = fp.metrics.clean_components,
        total_components = fp.metrics.total_components,
        total_lines = fp.metrics.total_lines,
        json_data = json_data
    )
}
