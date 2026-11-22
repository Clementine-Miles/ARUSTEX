import { fileURLToPath } from "node:url";

// NEXT_EXPORT=1 writes static pages to web/out. Leave it unset in next dev to keep API rewrites and hot
// reloading.
const isExport = process.env.NEXT_EXPORT === "1";
// Mock mode runs entirely in the frontend without a backend.
const isMock = process.env.NEXT_PUBLIC_MOCK === "1";

/** @type {import('next').NextConfig} */
const nextConfig = {
  // Pin the project root so a parent lockfile cannot affect asset paths.
  turbopack: { root: fileURLToPath(new URL(".", import.meta.url)) },
  reactCompiler: true,
  // Allow LAN development and HMR from arbitrary IPv4 addresses. Next requires segmented patterns instead of a
  // bare wildcard.
  allowedDevOrigins: ["*.*.*.*"],
  compiler: {
    removeConsole: process.env.NODE_ENV === "production",
  },
  ...(isExport
    ? {
        // Export each route as <route>/index.html with unoptimized images and no Node runtime.
        output: "export",
        images: { unoptimized: true },
        trailingSlash: true,
      }
    : isMock
      ? {
          // Mock mode needs no API rewrite.
          images: { unoptimized: true },
        }
      : {
          // Proxy development API requests to the Go backend. AUTOPENTEST_API overrides the default port 8787.
          async rewrites() {
            const backend = process.env.AUTOPENTEST_API ?? "http://localhost:8787";
            return [{ source: "/api/:path*", destination: `${backend}/api/:path*` }];
          },
        }),
};

export default nextConfig;
