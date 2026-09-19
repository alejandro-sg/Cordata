// crypto.randomUUID() is a Web Crypto API method restricted to secure
// contexts (HTTPS, or the localhost/127.0.0.1 origin the dev server was
// started on) — undefined everywhere else. This app is explicitly meant to
// also be opened over plain http:// from another device on the LAN (see
// next.config.js's allowedDevOrigins comment), which is exactly an insecure
// context: calling crypto.randomUUID() there throws "crypto.randomUUID is
// not a function", crashing page.tsx before it ever renders.
//
// crypto.getRandomValues(), unlike randomUUID(), IS available in every
// context per spec — build a real v4 UUID from it instead of dropping
// straight to Math.random(), since nothing about being on an insecure
// origin should make these ids weaker than they need to be.
export function randomId(): string {
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  if (typeof crypto !== "undefined" && typeof crypto.getRandomValues === "function") {
    const bytes = crypto.getRandomValues(new Uint8Array(16));
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // variant 10xx
    const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  }
  // Last-resort fallback — not cryptographically random, but every call
  // site here only needs a locally-unique id (a thread id, a message id, a
  // postMessage request id), never a security token.
  return "xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx".replace(/[xy]/g, (c) => {
    const r = (Math.random() * 16) | 0;
    const v = c === "x" ? r : (r & 0x3) | 0x8;
    return v.toString(16);
  });
}
