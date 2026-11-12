// A Ghostty window gaining the focus may show skiff sessions: systemd runs `skiff seen-window <pid>`
// (the skiff-seen@ unit), which marks them seen. KWin scripts cannot run commands themselves.
workspace.windowActivated.connect(function (window) {
    if (!window || String(window.resourceClass).indexOf("ghostty") < 0) {
        return;
    }
    callDBus(
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartUnit",
        "skiff-seen@" + window.pid + ".service",
        "replace"
    );
});
