// ==UserScript==
// @name         Onshape 3D mouse on Linux (spacemouse-onshape)
// @description  Onshape only looks for a 3D mouse driver on Windows and macOS. This reports the platform as Windows so it connects to spacemouse-onshape at https://127.51.68.120:8181.
// @match        https://cad.onshape.com/*
// @match        https://*.onshape.com/*
// @run-at       document-start
// @grant        none
// @version      0.1.0
// @license      MIT OR Apache-2.0
// ==/UserScript==

Object.defineProperty(Navigator.prototype, "platform", { get: () => "Win32", configurable: true });
