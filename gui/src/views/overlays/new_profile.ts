import {hide_overlay, passwordProblems, updateOverlay} from "../../utils.ts";
import {createProfile} from "../../ipc.ts";

export function renderNewProfileOverlay() {
    const template = document.createElement("template");
    template.innerHTML = `
<div id="new_profile">
    <h2>New Profile</h2>
    <div id="choices">
        <label for="password">Profile Password</label>
        <input type="password" id="password"/>
        <div style="padding-bottom: 10px; display:block; width: 100%;"></div>
        <label for="password2">Retype Password</label>
        <input type="password" id="password2" />
        <ul id="pw-reqs" class="reqs">
            <li id="len" class="invalid">At least 10, at most 128 characters</li>
            <li id="lower" class="invalid">At least one lowercase letter</li>
            <li id="upper" class="invalid">At least one uppercase letter</li>
            <li id="digit" class="invalid">At least one number</li>
            <li id="symbol" class="invalid">At least one symbol (e.g., !@#$)</li>
            <li id="spaces" class="invalid">No spaces or control characters</li>
            <li id="repeats" class="invalid">No 3+ repeated characters in a row</li>
            <li id="sequence" class="invalid">No obvious sequences (abcd / 1234)</li>
            <li id="common" class="invalid">Not a common/guessable word</li>
        </ul>
        <button id="new_profile_button">Generate</button>
        <button id="import_profile_button">Import From Mnemonic</button>
    </div>
</div>
`;
    const content = template.content;

    const password = content.querySelector<HTMLInputElement>("#password")!;
    const password2 = content.querySelector<HTMLInputElement>("#password2")!;
    const new_profile_button = content.querySelector<HTMLButtonElement>("#new_profile_button")!;
    const import_profile_button = content.querySelector<HTMLButtonElement>("#import_profile_button")!;
    const check_passwords = () => {
        let problems = passwordProblems(password.value);
            if (problems.length === 0 && password.value === password2.value) {
            console.log("Password is Valid");
            password2.classList.remove("invalid")
            new_profile_button.disabled = false;
        } else {
            console.log("Passwords Not Valid");
            new_profile_button.disabled = true;
            if (password.value === password2.value) {
                password2.classList.add("invalid")
            }
            document.querySelectorAll<HTMLUListElement>("#pw-reqs li")!.forEach((el: HTMLUListElement) => {
                if (problems.indexOf(el.id) > -1) {
                    el.classList.add("invalid")
                } else {
                    el.classList.remove("invalid")
                }
            });
        }
    };
    password.addEventListener("keyup", (_) => {
        check_passwords()
    });
    password2.addEventListener("keyup", (_) => {
        check_passwords()
    });
    new_profile_button.addEventListener("click", async (_) => {
        if (password.value === password2.value) {
            await createProfile(password.value);
            hide_overlay()
        }
    });
    import_profile_button.addEventListener("click", async (_) => {
        if (password.value === password2.value) {

        }
    });
    updateOverlay(content)
}