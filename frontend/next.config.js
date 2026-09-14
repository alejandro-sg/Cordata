/** @type {import('next').NextConfig} */
const nextConfig = {
  // Lean self-contained build (.next/standalone) for the Docker image this
  // ships in on the mini — bundles only the deps actually used at runtime,
  // instead of needing node_modules copied into the image separately.
  output: "standalone",
};

module.exports = nextConfig;
