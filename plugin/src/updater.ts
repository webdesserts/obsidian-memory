import { Notice, Plugin, requestUrl } from "obsidian";
import { log } from "./logger";

/** How often to check for updates (24 hours) */
const UPDATE_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;

/** localStorage key for last update check timestamp */
const LAST_UPDATE_CHECK_KEY = "p2p-sync-last-update-check";

/** GitHub API endpoint for latest release */
const RELEASES_URL =
  "https://api.github.com/repos/webdesserts/obsidian-memory/releases/latest";

/** Maps release asset names to local file names */
const ASSET_FILE_MAP: Record<string, string> = {
  "obsidian-plugin-main.js": "main.js",
  "obsidian-plugin-manifest.json": "manifest.json",
};

/**
 * Fire-and-forget update check called from plugin onload().
 * Downloads newer versions from GitHub releases with SHA-256 verification.
 */
export function checkForUpdates(plugin: Plugin): void {
  // Rate limit: once per 24h
  const lastCheck = localStorage.getItem(LAST_UPDATE_CHECK_KEY);
  if (lastCheck) {
    const elapsed = Date.now() - parseInt(lastCheck, 10);
    if (elapsed < UPDATE_CHECK_INTERVAL_MS) {
      log.debug(
        `Skipping update check (${Math.round(elapsed / 3600000)}h since last)`
      );
      return;
    }
  }

  // Track whether plugin has been unloaded during the async operation
  let unloaded = false;
  const originalOnunload = plugin.onunload.bind(plugin);
  plugin.register(() => {
    unloaded = true;
  });

  doUpdateCheck(plugin, () => unloaded).catch((err) => {
    console.warn("P2P Sync: update check failed:", err);
  });
}

async function doUpdateCheck(
  plugin: Plugin,
  isUnloaded: () => boolean
): Promise<void> {
  // Fetch latest release metadata
  let releaseData: any;
  try {
    const response = await requestUrl({
      url: RELEASES_URL,
      headers: { Accept: "application/vnd.github.v3+json" },
    });
    releaseData = response.json;
  } catch (err: any) {
    if (err?.status === 403 || err?.status === 429) {
      console.warn("P2P Sync: GitHub rate limit exceeded during update check");
    }
    return;
  }

  // Compare versions (strip leading 'v')
  const latestVersion = (releaseData.tag_name ?? "").replace(/^v/, "");
  const currentVersion = plugin.manifest.version;

  if (!latestVersion || !isNewerVersion(latestVersion, currentVersion)) {
    log.debug(`Up to date (current: ${currentVersion}, latest: ${latestVersion})`);
    localStorage.setItem(LAST_UPDATE_CHECK_KEY, String(Date.now()));
    return;
  }

  log.info(`Update available: ${currentVersion} → ${latestVersion}`);

  // Build asset URL map from release assets
  const assets: { name: string; url: string }[] = (
    releaseData.assets ?? []
  ).map((a: any) => ({
    name: a.name,
    url: a.browser_download_url,
  }));

  // Download checksums first
  const checksumAsset = assets.find(
    (a) => a.name === "obsidian-plugin-checksums.json"
  );
  if (!checksumAsset) {
    log.debug("No checksums asset in release, skipping update");
    localStorage.setItem(LAST_UPDATE_CHECK_KEY, String(Date.now()));
    return;
  }

  let checksums: Record<string, string>;
  try {
    const resp = await requestUrl({ url: checksumAsset.url });
    checksums = resp.json;
  } catch {
    return;
  }

  if (isUnloaded()) return;

  // Download each plugin asset
  const downloads: { localName: string; data: ArrayBuffer }[] = [];

  for (const [assetName, localName] of Object.entries(ASSET_FILE_MAP)) {
    const asset = assets.find((a) => a.name === assetName);
    if (!asset) {
      log.warn(`Missing release asset: ${assetName}`);
      return;
    }

    try {
      const resp = await requestUrl({ url: asset.url });
      downloads.push({ localName, data: resp.arrayBuffer });
    } catch {
      return;
    }

    if (isUnloaded()) return;
  }

  // Verify SHA-256 checksums
  for (const dl of downloads) {
    // Find the asset name for this local name
    const assetName = Object.entries(ASSET_FILE_MAP).find(
      ([, local]) => local === dl.localName
    )?.[0];
    if (!assetName) continue;

    const expectedHash = checksums[assetName];
    if (!expectedHash) {
      log.warn(`No checksum for ${assetName}`);
      return;
    }

    const actualHash = await sha256Hex(dl.data);
    if (actualHash !== expectedHash) {
      log.warn(
        `Checksum mismatch for ${assetName}: expected ${expectedHash}, got ${actualHash}`
      );
      return;
    }
  }

  if (isUnloaded()) return;

  // Atomic writes: .tmp suffix then rename
  // Write order: manifest.json → main.js (main.js last since it's the entry point)
  const pluginDir = getPluginDir(plugin);
  if (!pluginDir) return;

  const writeOrder = ["manifest.json", "main.js"];
  const sorted = downloads.sort(
    (a, b) => writeOrder.indexOf(a.localName) - writeOrder.indexOf(b.localName)
  );

  for (const dl of sorted) {
    if (isUnloaded()) return;

    const filePath = `${pluginDir}/${dl.localName}`;
    const tmpPath = `${filePath}.tmp`;
    const text = new TextDecoder().decode(dl.data);

    try {
      await plugin.app.vault.adapter.write(tmpPath, text);
      // Obsidian's adapter doesn't have rename, so write directly
      await plugin.app.vault.adapter.write(filePath, text);
      // Clean up tmp
      if (await plugin.app.vault.adapter.exists(tmpPath)) {
        await plugin.app.vault.adapter.remove(tmpPath);
      }
    } catch (err) {
      log.warn(`Failed to write ${dl.localName}:`, err);
      return;
    }
  }

  localStorage.setItem(LAST_UPDATE_CHECK_KEY, String(Date.now()));
  new Notice(`P2P Sync: Updated to v${latestVersion}! Restart Obsidian to apply.`);
  log.info(`Updated to v${latestVersion}`);
}

/** Compare semver strings. Returns true if `a` is newer than `b`. */
function isNewerVersion(a: string, b: string): boolean {
  const pa = a.split(".").map(Number);
  const pb = b.split(".").map(Number);
  for (let i = 0; i < 3; i++) {
    const va = pa[i] ?? 0;
    const vb = pb[i] ?? 0;
    if (va > vb) return true;
    if (va < vb) return false;
  }
  return false;
}

/** SHA-256 hash of an ArrayBuffer, returned as lowercase hex string. */
async function sha256Hex(data: ArrayBuffer): Promise<string> {
  const hash = await crypto.subtle.digest("SHA-256", data);
  return Array.from(new Uint8Array(hash))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/** Get the plugin directory path relative to vault root. */
function getPluginDir(plugin: Plugin): string | null {
  // Plugin dir is always at .obsidian/plugins/<plugin-id>
  return `.obsidian/plugins/${plugin.manifest.id}`;
}
