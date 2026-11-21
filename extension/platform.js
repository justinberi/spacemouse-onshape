// Runs in the page before Onshape's own scripts (see manifest.json): Onshape only looks for a 3D
// mouse driver on Windows and macOS, so report Windows and it connects to spacemouse-onshape.
Object.defineProperty(Navigator.prototype, "platform", { get: () => "Win32", configurable: true });
