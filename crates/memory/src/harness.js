// Bootstrap harness for P2P Sync plugin.
// This file is written by `memory obsidian install` and will be replaced
// on first load by the full plugin downloaded from GitHub releases.

const { Plugin, Notice, Platform, requestUrl } = require("obsidian");

const RELEASES_URL =
  "https://api.github.com/repos/webdesserts/obsidian-memory/releases/latest";

// Release asset names → local file names
const ASSET_MAP = {
  "obsidian-plugin-main.js": "main.js",
  "obsidian-plugin-manifest.json": "manifest.json",
  "obsidian-plugin-ws-server.js": "ws-server.js",
};

async function sha256Hex(data) {
  const hash = await crypto.subtle.digest(
    "SHA-256",
    typeof data === "string" ? new TextEncoder().encode(data) : data
  );
  return Array.from(new Uint8Array(hash))
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

module.exports = class BootstrapPlugin extends Plugin {
  async onload() {
    const statusEl = Platform.isDesktop ? this.addStatusBarItem() : null;
    const setStatus = (text) => statusEl?.setText(`P2P Sync: ${text}`);

    setStatus("Downloading...");
    new Notice("P2P Sync: Downloading plugin...");

    try {
      // Fetch latest release
      let releaseData;
      try {
        const resp = await requestUrl({
          url: RELEASES_URL,
          headers: { Accept: "application/vnd.github.v3+json" },
        });
        releaseData = resp.json;
      } catch (err) {
        if (err?.status === 403 || err?.status === 429) {
          throw new Error("GitHub rate limit exceeded, try again later");
        }
        throw new Error(`Failed to fetch release: ${err?.message || err}`);
      }

      const assets = (releaseData.assets || []).map((a) => ({
        name: a.name,
        url: a.browser_download_url,
      }));

      // Download checksums
      const checksumAsset = assets.find(
        (a) => a.name === "obsidian-plugin-checksums.json"
      );
      if (!checksumAsset) throw new Error("No checksums in release");

      const checksumResp = await requestUrl({ url: checksumAsset.url });
      const checksums = checksumResp.json;

      // Download each asset
      const downloads = [];
      for (const [assetName, localName] of Object.entries(ASSET_MAP)) {
        const asset = assets.find((a) => a.name === assetName);
        if (!asset) {
          // ws-server.js only needed on desktop
          if (assetName === "obsidian-plugin-ws-server.js" && Platform.isMobile)
            continue;
          if (assetName === "obsidian-plugin-ws-server.js") continue; // optional
          throw new Error(`Missing release asset: ${assetName}`);
        }

        const resp = await requestUrl({ url: asset.url });
        const data = resp.arrayBuffer;

        // Verify checksum
        const expected = checksums[assetName];
        if (!expected) throw new Error(`No checksum for ${assetName}`);
        const actual = await sha256Hex(data);
        if (actual !== expected) {
          throw new Error(
            `Checksum mismatch for ${assetName}: expected ${expected}, got ${actual}`
          );
        }

        downloads.push({ localName, text: new TextDecoder().decode(data) });
      }

      // Atomic writes: .tmp then overwrite. Order: ws-server → manifest → main (last)
      const pluginDir = `.obsidian/plugins/${this.manifest.id}`;
      const writeOrder = ["ws-server.js", "manifest.json", "main.js"];
      downloads.sort(
        (a, b) =>
          writeOrder.indexOf(a.localName) - writeOrder.indexOf(b.localName)
      );

      for (const dl of downloads) {
        const path = `${pluginDir}/${dl.localName}`;
        const tmp = `${path}.tmp`;
        await this.app.vault.adapter.write(tmp, dl.text);
        await this.app.vault.adapter.write(path, dl.text);
        if (await this.app.vault.adapter.exists(tmp)) {
          await this.app.vault.adapter.remove(tmp);
        }
      }

      setStatus("Installed");
      new Notice(
        "P2P Sync installed! Restart Obsidian to activate.",
        10000
      );
    } catch (err) {
      const msg = err?.message || String(err);
      console.error("P2P Sync bootstrap error:", msg);
      setStatus("Error");
      new Notice(`P2P Sync: ${msg}`, 10000);
    }
  }
};
