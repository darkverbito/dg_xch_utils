import type { Profile } from "@bindings/Profile";
import { escapeHtml } from "../utils.ts";
import {renderNewProfileOverlay} from "./overlays/new_profile.ts";
import {imageLifehash} from "../ipc.ts";

export async function renderProfiles(profiles: Profile[]) {
    const mount = document.getElementById("left_nav")!;
    let images: [Profile, HTMLImageElement][] = []
    for (const profile of profiles) {
        const image = await imageLifehash(profile);
        let canvas = document.createElement("canvas");
        canvas.height = image.height;
        canvas.width = image.width;
        const ctx = canvas.getContext("2d")!;
        const u8 = new Uint8ClampedArray(image.data); // RGBA
        const imageData = new ImageData(u8, image.width, image.height);
        ctx.putImageData(imageData, 0, 0);
        const img = new Image();
        const blob: Blob = await new Promise((res) =>
            canvas.toBlob((b) => res(b!), "image/png")
        );
        img.src =  URL.createObjectURL(blob);
        images.push([profile, img])
    }
    const items = images
                .map(
                    ([p, img]): string => {
                        return `
        <li class="profile unlocked" data-id="${p.id}">
            <div class="profile_image"><img src="${img.src}" /></div>
            <div class="profile_info">
                <div class="name">${escapeHtml(p.name)}</div>
                <div class="desc">${escapeHtml(p.description ?? "")}</div>
            </div>
            <div class="actions">
                <button id="unlock">Unlock</button>
                <button id="edit">Edit</button>
            </div>
        </li>`;
                    }
                );
    mount.innerHTML = `
      <h3>Profiles</h3>
      <ul id="profiles-list">${items}</ul>
      <div class="actions">
        <button id="new-profile">+ New Profile</button>
      </div>
  `;

    // Example click handler (optional)
    const ul = mount.querySelector<HTMLUListElement>("#profiles-list")!;
    ul.addEventListener("click", (e) => {
        const li = (e.target as HTMLElement).closest<HTMLLIElement>("li.profile");
        if (!li) return;
        const id = Number(li.dataset.id);
        console.log("Selected profile", id);
    });

    // Example click handler (optional)
    const new_button = mount.querySelector<HTMLButtonElement>("#new-profile")!;
    new_button.addEventListener("click", (_) => {
        renderNewProfileOverlay()
    });
}