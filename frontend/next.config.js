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

  // Dev-server-only (next dev) protection Next.js 16 added: a request whose
  // Origin header isn't localhost/the hostname the server was started with
  // gets blocked, even though main.py's backend and this frontend's own dev
  // server both already bind 0.0.0.0 and are otherwise reachable — this was
  // the actual cause of "backend/frontend are on 0.0.0.0 but another device
  // on the LAN still can't load the page," not a firewall or bind-address
  // issue. Wildcarded to the whole /24 rather than one hardcoded IP, since
  // this machine's LAN address is DHCP-assigned and can change. Irrelevant
  // to the standalone production build (output: "standalone" above) this
  // app also ships as a Docker image on the mini — allowedDevOrigins only
  // applies to `next dev`.
  allowedDevOrigins: ["192.168.10.*"],

  async headers() {
    return [
      { source: "/((?!api/games|api/sandbox).*)", headers: [...GENERAL_HEADERS, ...FRAME_ANCESTORS_NONE_HEADERS] },
      { source: "/api/games/:path*", headers: GENERAL_HEADERS },
      { source: "/api/sandbox/:path*", headers: GENERAL_HEADERS },
    ];
  },
};

module.exports = nextConfig;
