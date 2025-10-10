export function escapeHtml(s: string) {
    // simple XSS guard for names/descriptions coming from disk
    return s
        .replaceAll("&", "&amp;")
        .replaceAll("<", "&lt;")
        .replaceAll(">", "&gt;")
        .replaceAll('"', "&quot;")
        .replaceAll("'", "&#39;");
}

export function hide_overlay() {
    document.getElementById("overlay")!.style.display = "none";
}

export function show_overlay() {
    document.getElementById("overlay")!.style.display = "flex";
}

export function updateOverlay(html: Node | string) {
    const mount = document.getElementById("overlay_content")!;
    hide_overlay();
    mount.innerHTML = ``;
    mount.append(html);
    show_overlay();
}

const BAD_WORDS = [
    "password", "qwerty", "letmein", "admin", "welcome", "iloveyou",
    "abc123", "123456", "12345678", "111111", "monkey"
];

function hasLongSequence(s: string, runLen = 4): boolean {
    if (s.length < runLen) return false;
    const chars = [...s.toLowerCase()];
    // check only letters/digits for sequence detection
    const codes = chars.map(c => {
        if (c >= "a" && c <= "z") return c.charCodeAt(0);
        if (c >= "0" && c <= "9") return 1000 + c.charCodeAt(0); // keep digits separate band
        return NaN;
    });

    let up = 1, down = 1;
    for (let i = 1; i < codes.length; i++) {
        const a = codes[i - 1], b = codes[i];
        if (Number.isNaN(a) || Number.isNaN(b)) { up = down = 1; continue; }
        if (b === a + 1) { up++; down = 1; }       // ascending (e.g., abcd, 1234)
        else if (b === a - 1) { down++; up = 1; }  // descending (e.g., dcba, 4321)
        else { up = down = 1; }
        if (up >= runLen || down >= runLen) return true;
    }
    return false;
}

export function passwordProblems(pw: string): string[] {
    const fails: string[] = [];
    // one "len" rule that covers both min & max
    const lenOk = pw.length >= 10 && pw.length <= 128;
    if (!lenOk) fails.push("len");
    if (!/[a-z]/.test(pw)) fails.push("lower");
    if (!/[A-Z]/.test(pw)) fails.push("upper");
    if (!/[0-9]/.test(pw)) fails.push("digit");
    if (!/[^\w\s]/.test(pw)) fails.push("symbol");
    // spaces/control combined into one "spaces" rule
    const hasSpaces = /\s/.test(pw);
    const hasControl = /[\x00-\x1F\x7F]/.test(pw);
    if (hasSpaces || hasControl) fails.push("spaces");
    if (/(.)\1{2,}/.test(pw)) fails.push("repeats");
    if (hasLongSequence(pw)) fails.push("sequence");
    const lower = pw.toLowerCase();
    if (BAD_WORDS.some(w => lower.includes(w))) fails.push("common");
    return fails;
}
