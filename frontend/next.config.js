// frame-ancestors 'none': this app's own pages must never be embeddable in
// someone else's iframe (clickjacking) — the reverse direction (this app
// embedding its own generative-UI/game content in ITS OWN iframe) is
// unaffected, since frame-ancestors controls who may frame a response, not
// what that response may itself embed. Scoped OFF of /api/games and
// /api/sandbox below for exactly that reason: those routes exist
// specifically to be loaded into GameSandbox.tsx's/the generative-UI
// renderer's own <iframe sandbox="allow-scripts">, and a page can't apply
// two different frame-ancestors values to two different requests for the
// same path, so the general rule below has to name its own exceptions.
const FRAME_ANCESTORS_NONE_HEADERS = [
  { key: "X-Frame-Options", value: "DENY" },
  { key: "Content-Security-Policy", value: "frame-ancestors 'none'" },
];

// Safe on every response regardless of framing — neither depends on nor
// affects the /api/games, /api/sandbox exception above.
const GENERAL_HEADERS = [
  { key: "X-Content-Type-Options", value: "nosniff" },
  { key: "Referrer-Policy", value: "no-referrer" },
];

/** @type {import('next').NextConfig} */
const nextConfig = {
  // Lean self-contained build (.next/standalone) for the Docker image this
  // ships in on the mini — bundles only the deps actually used at runtime,
  // instead of needing node_modules copied into the image separately.
  output: "standalone",

  async headers() {
    return [
      { source: "/((?!api/games|api/sandbox).*)", headers: [...GENERAL_HEADERS, ...FRAME_ANCESTORS_NONE_HEADERS] },
      { source: "/api/games/:path*", headers: GENERAL_HEADERS },
      { source: "/api/sandbox/:path*", headers: GENERAL_HEADERS },
    ];
  },
};

module.exports = nextConfig;
