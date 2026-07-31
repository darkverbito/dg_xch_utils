import { invoke } from "@tauri-apps/api/core";
import type { ProfileList } from "@bindings/ProfileList";
import type { Profile } from "@bindings/Profile";
import type {RgbaImage} from "@bindings/RgbaImage.ts";

export async function listProfiles(): Promise<ProfileList[]> {
    const res = await invoke<ProfileList>("list_profiles");
    return res.profiles;
}

export async function createProfile(password: String): Promise<Profile> {
    return await invoke<Profile>("create_profile", { mnemonic: null, password: password });
}

export async function importProfile(mnemonic: String, password: String): Promise<Profile> {
    return await invoke<Profile>("create_profile", { mnemonic: mnemonic, password: password });
}

export async function deleteProfile(profile_id: Number): Promise<null> {
    return await invoke<null>("delete_profile", { profile_id: profile_id})
}

export async function imageLifehash(profile: Profile): Promise<RgbaImage> {
    return await invoke<RgbaImage>("image_lifehash", { hash: profile.image_hash})
}