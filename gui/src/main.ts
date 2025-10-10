import './style.css'
import { listProfiles } from "./ipc";
import { renderProfiles } from "./views/profile_list.ts";
import {hide_overlay} from "./utils.ts";

document.querySelector<HTMLDivElement>('#app')!.innerHTML = `
<div id="overlay"><div id="blanket"></div><div id="overlay_content"><!--Dynamically Rendered--></div></div>
<div id="header">
    <div id="top_nav">
        <div id="node" class="nav-item active">Node</div>
        <div id="data_layer" class="nav-item">Data Layer</div>
        <div id="simulator" class="nav-item dev">Simulator</div>
        <div id="tools" class="nav-item dev">Tools</div>
        <div id="settings" class="nav-item">Settings</div>
        <div id="color_mode" class="nav-item toggle"></div>
    </div>
</div>
<div id="content">
    <div id="left_nav"><!--Dynamically Rendered--></div>
    <div id="main"><!--Dynamically Rendered--></div>
</div>
`
async function init() {
    try {
        document.getElementById("blanket")!.addEventListener("click", (_) => {
            hide_overlay()
        });
        const profiles = await listProfiles();
        renderProfiles(profiles);
    } catch (err) {
        console.error("Failed to load profiles:", err);
    }
}

if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", () => init(), { once: true });
} else {
    init();
}
