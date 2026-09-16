"use strict";
// Static trusted shell on a separate, credential-free site. No URL parameters,
// fetch, storage, innerHTML, dynamic script, downloads or parent navigation.
(() => {
  const parentOrigin = document.querySelector(
    'meta[name="br-parent-origin"]',
  ).content;
  const video = document.getElementById("video"),
    status = document.getElementById("status");
  let accepted = false,
    port = null,
    session = null,
    parts = [],
    received = 0,
    ordinal = 0,
    blob = null,
    closed = false,
    finishing = false;
  const send = (state, extra = {}) =>
    port?.postMessage({
      contract: "isolated-mp4-viewer-v1",
      session_id: session.session_id,
      state,
      ...extra,
    });
  const stop = (state) => {
    if (closed) return;
    closed = true;
    video.pause();
    video.removeAttribute("src");
    video.load();
    if (blob) URL.revokeObjectURL(blob);
    parts = [];
    received = 0;
    status.textContent =
      state === "expired"
        ? "审阅已过期，内容已清除。"
        : "内容校验或播放失败，不能据此放行。";
    send(state);
    port?.close();
  };
  window.addEventListener("message", function init(e) {
    if (
      accepted ||
      window.parent === window ||
      e.source !== window.parent ||
      e.origin !== parentOrigin ||
      e.ports.length !== 1
    )
      return;
    const m = e.data;
    if (
      !m ||
      m.contract !== "isolated-mp4-viewer-v1" ||
      typeof m.session_id !== "string" ||
      !/^[0-9a-f-]{36}$/.test(m.session_id) ||
      !Number.isInteger(m.byte_length) ||
      m.byte_length < 1 ||
      m.byte_length > 67108864 ||
      m.chunk_count !== Math.ceil(m.byte_length / 4194304) ||
      typeof m.sha256 !== "string" ||
      !/^[a-f0-9]{64}$/.test(m.sha256)
    )
      return;
    const ttl = Date.parse(m.expires_at) - Date.now();
    if (!Number.isFinite(ttl) || ttl <= 0 || ttl > 300000) return;
    accepted = true;
    session = m;
    port = e.ports[0];
    setTimeout(() => stop("expired"), ttl);
    video.addEventListener("error", () => stop("failed"));
    video.addEventListener(
      "loadeddata",
      () => {
        if (closed) return;
        if (
          !Number.isFinite(video.duration) ||
          video.duration <= 0 ||
          video.duration > 300 ||
          video.videoWidth <= 0 ||
          video.videoHeight <= 0 ||
          video.videoWidth * video.videoHeight > 8294400
        ) {
          stop("failed");
          return;
        }
        status.textContent =
          "完整内容摘要一致。请实际审阅视频后回到控制台确认。";
        send("rendered", { sha256: session.sha256 });
      },
      { once: true },
    );
    port.onmessage = async (e) => {
      if (closed) return;
      const v = e.data;
      if (
        v?.type === "chunk" &&
        !finishing &&
        v.ordinal === ordinal &&
        v.bytes instanceof ArrayBuffer &&
        v.bytes.byteLength ===
          Math.min(4194304, session.byte_length - received) &&
        ordinal < session.chunk_count
      ) {
        parts.push(v.bytes);
        received += v.bytes.byteLength;
        ordinal++;
        return;
      }
      if (
        v?.type !== "finish" ||
        finishing ||
        received !== session.byte_length ||
        ordinal !== session.chunk_count
      ) {
        stop("failed");
        return;
      }
      finishing = true;
      try {
        const all = new Uint8Array(received);
        let offset = 0;
        for (const part of parts) {
          all.set(new Uint8Array(part), offset);
          offset += part.byteLength;
        }
        parts = [];
        const digest = Array.from(
          new Uint8Array(await crypto.subtle.digest("SHA-256", all)),
        )
          .map((v) => v.toString(16).padStart(2, "0"))
          .join("");
        if (closed) return;
        if (digest !== session.sha256) {
          stop("failed");
          return;
        }
        blob = URL.createObjectURL(new Blob([all], { type: "video/mp4" }));
        video.preload = "auto";
        video.src = blob;
        video.load();
        setTimeout(() => {
          if (!closed && video.readyState < 2) stop("failed");
        }, 15000);
      } catch {
        stop("failed");
      }
    };
    send("ready");
  });
  window.addEventListener("pagehide", () => stop("expired"));
})();
