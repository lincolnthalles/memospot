/*
 * This script runs before `Tauri build` step.
 * deno run -A ./build-scripts/downloadMemosBuildsHook.ts
 */

import type { GitHubAsset, GitHubRelease } from "./downloadMemosBuildsHook.d.ts";
import { findRepositoryRoot } from "./common.ts";
import { crypto, encodeHex, existsSync, globToRegExp } from "../deps.ts";

import { Readable } from "../deps.ts";
import { finished } from "../deps.ts";
import * as fs from "node:fs";

// @deno-types="../deps.ts"
import { decompress } from "../deps.ts";

/**
 * Convert a GOOS-GOARCH build file name to a Rust target triple.
 *
 * Sample target triples:
 *
 * - `x86_64-pc-windows-msvc`
 * - `x86_64-unknown-linux-gnu`
 * - `x86_64-apple-darwin`
 * - `aarch64-apple-darwin`
 *
 * @param file The file name.
 * @returns The target triple.
 */
export function makeTripletFromFileName(file: string): string {
    const os = (() => {
        const oses = ["darwin", "linux", "windows"];

        for (const os of oses) {
            if (file.includes(os)) {
                return os;
            }
        }

        return "unknown";
    })();

    const platform = (() => {
        const platformMap: Record<string, string> = {
            windows: "pc",
            linux: "unknown",
            darwin: "apple",
        };

        for (const [key, value] of Object.entries(platformMap)) {
            if (file.includes(key)) {
                return value;
            }
        }

        return "unknown";
    })();

    const arch = (() => {
        const archMap: Record<string, string> = {
            x86_64: "x86_64",
            x64: "x86_64",
            x86: "i686",
            "386": "i686",
            arm64: "aarch64",
            aarch64: "aarch64",
            riscv64: "riscv64gc",
        };

        for (const [key, value] of Object.entries(archMap)) {
            if (file.includes(key)) {
                return value;
            }
        }

        return "unknown";
    })();

    const variant = (() => {
        const variantMap: Record<string, string> = {
            windows: "msvc",
            linux: "gnu",
        };

        for (const [key, value] of Object.entries(variantMap)) {
            if (file.includes(key)) {
                return value;
            }
        }

        return "";
    })();

    const triplet = [arch, platform, os, variant].join("-");
    if (triplet.endsWith("-")) {
        return triplet.slice(0, -1);
    }
    return triplet;
}

async function downloadServerBinaries() {
    const repo = "lincolnthalles/memos-builds";
    const repoUrl = `https://api.github.com/repos/${repo}/releases/latest`;

    // Match only platforms that Memospot also supports
    const downloadFilesGlob = [
        "memos-*-darwin-arm64.tar.gz",
        "memos-*-darwin-x86_64.tar.gz",
        "memos-*-linux-x86_64.tar.gz",
        "memos-*-windows-x86_64.zip",
    ];

    // fetch data from github api
    const response = await fetch(repoUrl);
    const json = (await response.json()) as GitHubRelease;
    const assets = json.assets as GitHubAsset[];

    if (!assets || assets.length == 0) {
        throw new Error("Failed to fetch assets");
    }
    const tag = json.tag_name;

    const sha256sums = assets.find((asset) => {
        return asset.name.endsWith("SHA256SUMS.txt");
    })?.browser_download_url;

    if (!sha256sums) {
        throw new Error("Failed to find SHA256SUMS.txt");
    }

    console.log(`Latest ${repo} tag: ${tag}`);

    const selectedFiles: GitHubAsset[] = [];
    for (const asset of assets) {
        // glob-like matching
        for (const mask of downloadFilesGlob) {
            const regex = new RegExp(globToRegExp(mask));
            if (asset.name.match(regex)) {
                selectedFiles.push(asset);
            }
        }
    }

    if (selectedFiles.length == 0) {
        throw new Error("Failed to match files");
    } else {
        console.log(`Matched ${selectedFiles.length} files`);
    }

    // download files
    for (const file of selectedFiles) {
        const downloadUrl = file.browser_download_url as string;
        const fileName = file.name;
        const filePath = `./server-dist/${fileName}`;
        const fileExists = existsSync(filePath, { isFile: true });
        if (!fileExists) {
            console.log(`Downloading ${fileName}...`);
            const res = await fetch(downloadUrl);
            const fileStream = fs.createWriteStream(filePath, { flags: "wx" });
            if (!res.body) {
                throw new Error("No response body");
            }
            await finished(Readable.fromWeb(res.body as any).pipe(fileStream));
        } else {
            console.log(`File ${fileName} already exists and will be reused.`);
        }
    }

    // check hashes via memos_SHA256SUMS.txt
    const sha256response = await fetch(sha256sums);
    const data = await sha256response.text();
    const lines = data.split("\n");
    const fileHashes: Record<string, string> = {};
    for (const line of lines) {
        if (line.length == 0) {
            continue;
        }
        const elements = line.split("  ");
        const hash = elements[0].trim();
        const fileName = elements[1].trim();
        fileHashes[fileName] = hash;
    }

    for (const file of selectedFiles) {
        const fileName = file.name;
        console.log(`Checking hash for ${fileName}...`);

        const filePath = `./server-dist/${fileName}`;
        // resolve path
        const fileBuffer = Deno.readFileSync(filePath);

        const fileHash = await crypto.subtle.digest("SHA-256", fileBuffer).then((hash) => {
            return encodeHex(new Uint8Array(hash));
        });

        console.log(`Hash: ${fileHash}`);
        if (fileHash !== fileHashes[fileName]) {
            throw new Error(`Hash mismatch for ${fileName}`);
        }
    }

    // extract files
    const extractDir = "./server-dist/extracted";
    const extractDirExists = existsSync(extractDir, { isDirectory: true });
    if (!extractDirExists) {
        Deno.mkdirSync(extractDir);
    }

    for (const file of selectedFiles) {
        const fileName = file.name;
        const filePath = `./server-dist/${fileName}`;
        if (fileName.endsWith(".zip") || fileName.endsWith(".tar.gz")) {
            console.log(`Extracting ${fileName}...`);
            await decompress(filePath, extractDir).then((files) => {
                console.log(`Extracted ${files.length} files`);
            });
        }

        const exe = fileName.includes("windows") ? ".exe" : "";

        const triplet = makeTripletFromFileName(fileName);
        Deno.renameSync(`${extractDir}/memos${exe}`, `./server-dist/memos-${triplet}${exe}`);
        // chmod +x downloaded file
        if (Deno.build.os !== "windows") {
            Deno.chmodSync(`./server-dist/memos-${triplet}${exe}`, 0o755);
        }

        // move front-end dist folder, only once, as it's the same for all platforms
        if (!existsSync("./server-dist/dist", { isDirectory: true })) {
            Deno.renameSync(`${extractDir}/dist`, `./server-dist/dist`);
        }

        Deno.removeSync(extractDir, { recursive: true });
        Deno.removeSync(filePath);
    }
}

async function main() {
    const repoRoot = findRepositoryRoot();
    console.log(`Repository root is \`${repoRoot}\``);
    Deno.chdir(repoRoot);
    console.log("Running pre-build hook `Download Memos Builds` ...");

    const serverDistDir = "./server-dist";
    const serverDistDirExists = existsSync(serverDistDir, { isDirectory: true });
    if (!serverDistDirExists) {
        Deno.mkdirSync(serverDistDir, { recursive: true, mode: 0o755 });
    }

    // remove a previous dist folder, if it exists
    if (serverDistDirExists && existsSync("./server-dist/dist", { isDirectory: true })) {
        Deno.removeSync("./server-dist/dist", { recursive: true });
    }

    await downloadServerBinaries();
}

main().catch((e) => {
    throw e;
});
