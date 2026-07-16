import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { check } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";

const updateBanner = document.querySelector<HTMLDivElement>("#update-banner")!;
const updateText = document.querySelector<HTMLSpanElement>("#update-text")!;
const updateBtn = document.querySelector<HTMLButtonElement>("#update-btn")!;

checkForUpdate();

async function checkForUpdate() {
  try {
    const update = await check();
    if (!update) return;

    updateText.textContent = `A new version (${update.version}) is available.`;
    updateBanner.classList.add("visible");

    updateBtn.addEventListener("click", async () => {
      updateBtn.disabled = true;
      updateText.textContent = `Downloading version ${update.version}...`;
      try {
        await update.downloadAndInstall();
        updateText.textContent = "Update installed. Restarting...";
        await relaunch();
      } catch (e) {
        updateBtn.disabled = false;
        updateText.textContent = `Update failed: ${e}`;
      }
    });
  } catch (e) {
    console.error("Update check failed:", e);
  }
}

const urlInput = document.querySelector<HTMLInputElement>("#url-input")!;
const formatSelect = document.querySelector<HTMLSelectElement>("#format-select")!;
const delaySelect = document.querySelector<HTMLSelectElement>("#delay-select")!;
const outputPath = document.querySelector<HTMLInputElement>("#output-path")!;
const browseBtn = document.querySelector<HTMLButtonElement>("#browse-btn")!;
const downloadBtn = document.querySelector<HTMLButtonElement>("#download-btn")!;
const log = document.querySelector<HTMLDivElement>("#log")!;

urlInput.addEventListener("focus", () => urlInput.select());

browseBtn.addEventListener("click", async () => {
  const selected = await open({ directory: true, multiple: false });
  if (selected) {
    outputPath.value = selected as string;
  }
});

downloadBtn.addEventListener("click", async () => {
  const url = urlInput.value.trim();
  const format = formatSelect.value;
  const delaySecs = parseInt(delaySelect.value, 10);
  const folder = outputPath.value.trim();

  if (!url) return alert("Please enter a URL.");
  if (!folder) return alert("Please choose an output folder.");

  log.classList.add("visible");
  log.textContent = "";
  downloadBtn.disabled = true;

  const unlistenProgress = await listen<string>("download-progress", (e) => {
    log.textContent += e.payload + "\n";
    log.scrollTop = log.scrollHeight;
  });

  const unlistenDone = await listen<string>("download-done", (e) => {
    log.textContent += "\n✓ " + e.payload;
    downloadBtn.disabled = false;
    unlistenProgress();
    unlistenDone();
    unlistenError();
  });

  const unlistenError = await listen<string>("download-error", (e) => {
    log.textContent += "\n✗ " + e.payload;
    downloadBtn.disabled = false;
    unlistenProgress();
    unlistenDone();
    unlistenError();
  });

  invoke("download", { url, format, outputPath: folder, delaySecs });
});
