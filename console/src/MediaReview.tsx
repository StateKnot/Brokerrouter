import { useEffect, useRef, useState } from "react";
import { z } from "zod";
import { ApiError, errorMessage, mediaChunk, request } from "./api";
import { reviewSchema } from "./approval-contracts";
import type { Review } from "./approval-contracts";
import type { Denied } from "./Management";
import { timestamp } from "./contracts";
import { time } from "./format";
const hash = z.string().regex(/^[a-f0-9]{64}$/);
export const mediaManifest = z
  .object({
    contract: z.literal("isolated-mp4-review-v1"),
    session_id: z.uuid(),
    approval_id: z.uuid(),
    expires_at: timestamp,
    output: z.object({
      contract: z.literal("current-turn-output-v1"),
      sha256: hash,
      byte_length: z.number().int().min(1).max(67108864),
      chunk_count: z.number().int().min(1).max(16),
      chunk_bytes: z.literal(4194304),
      expires_at: timestamp,
      scope: z.literal("current_turn_only"),
    }),
  })
  .refine(
    (v) =>
      v.output.chunk_count === Math.ceil(v.output.byte_length / 4194304) &&
      Date.parse(v.expires_at) <= Date.parse(v.output.expires_at),
  );
type Manifest = z.infer<typeof mediaManifest>;
export function previewUrl(
  origin: string | null,
  parent: string,
): string | null {
  if (!origin) return null;
  try {
    const u = new URL(origin),
      p = new URL(parent);
    return u.protocol === "https:" &&
      u.origin === origin &&
      u.hostname !== p.hostname
      ? `${origin}/review.html`
      : null;
  } catch {
    return null;
  }
}
type Props = {
  approvalId: string;
  previewOrigin: string | null;
  disabled: boolean;
  completed: boolean;
  onComplete: (r: Review) => void;
  denied: Denied;
};
export default function MediaReview({
  approvalId,
  previewOrigin,
  disabled,
  completed,
  onComplete,
  denied,
}: Props) {
  const [manifest, setManifest] = useState<Manifest | null>(null),
    [status, setStatus] = useState(""),
    [error, setError] = useState<unknown>(null),
    [ready, setReady] = useState(false),
    [busy, setBusy] = useState(false);
  const frame = useRef<HTMLIFrameElement | null>(null),
    control = useRef<AbortController | null>(null),
    port = useRef<MessagePort | null>(null),
    started = useRef(false),
    attempt = useRef(crypto.randomUUID());
  const url = previewUrl(previewOrigin, location.origin);
  function clear() {
    control.current?.abort();
    port.current?.close();
    port.current = null;
  }
  useEffect(() => () => clear(), []);
  useEffect(() => {
    if (!manifest) return;
    const t = setTimeout(
      () => {
        clear();
        setReady(false);
        setManifest(null);
        setStatus("媒体审阅已过期，播放器与内存内容已清除，请关闭并重新审阅。");
      },
      Math.max(0, Date.parse(manifest.expires_at) - Date.now()),
    );
    return () => clearTimeout(t);
  }, [manifest]);
  function fail(e: unknown) {
    clear();
    setReady(false);
    setManifest(null);
    setBusy(false);
    setError(e);
    if (e instanceof ApiError && (e.status === 401 || e.status === 403))
      denied(e);
  }
  async function start() {
    if (!url || busy || disabled || completed) return;
    clear();
    started.current = false;
    setError(null);
    setReady(false);
    setManifest(null);
    setBusy(true);
    setStatus("正在取得本人媒体审阅清单…");
    const c = new AbortController();
    control.current = c;
    try {
      const m = await request(
        "media-review/start",
        mediaManifest,
        { approval_id: approvalId, session_id: attempt.current },
        c.signal,
      );
      if (c.signal.aborted) return;
      if (
        m.approval_id !== approvalId ||
        m.session_id !== attempt.current ||
        Date.parse(m.expires_at) <= Date.now() ||
        Date.parse(m.expires_at) > Date.now() + 300_000
      )
        throw new ApiError(502, "invalid_response");
      setManifest(m);
    } catch (e) {
      if (!c.signal.aborted) fail(e);
    }
  }
  function loaded() {
    if (!manifest || !frame.current?.contentWindow || !control.current) return;
    if (started.current) {
      fail(new ApiError(502, "invalid_response"));
      return;
    }
    started.current = true;
    const c = control.current,
      m = manifest,
      channel = new MessageChannel();
    port.current = channel.port1;
    const timer = setTimeout(() => {
      if (!c.signal.aborted) fail(new ApiError(0, "request_timeout"));
    }, 10_000);
    let state = "initial";
    channel.port1.onmessage = async (e: MessageEvent) => {
      const v = e.data;
      if (c.signal.aborted) return;
      if (
        !v ||
        v.contract !== "isolated-mp4-viewer-v1" ||
        v.session_id !== m.session_id
      ) {
        clearTimeout(timer);
        fail(new ApiError(502, "invalid_response"));
        return;
      }
      if (v.state === "ready" && state === "initial") {
        state = "loading";
        clearTimeout(timer);
        setStatus("正在逐块加载视频并校验完整 SHA-256…");
        try {
          for (let ordinal = 0; ordinal < m.output.chunk_count; ordinal++) {
            const b = await mediaChunk(
              { session_id: m.session_id, ordinal },
              Math.min(4194304, m.output.byte_length - ordinal * 4194304),
              c.signal,
            );
            if (c.signal.aborted) return;
            channel.port1.postMessage({ type: "chunk", ordinal, bytes: b }, [
              b,
            ]);
          }
          if (!c.signal.aborted) {
            state = "verifying";
            channel.port1.postMessage({ type: "finish" });
          }
        } catch (e) {
          if (!c.signal.aborted) fail(e);
        }
      } else if (
        v.state === "rendered" &&
        state === "verifying" &&
        v.sha256 === m.output.sha256
      ) {
        state = "rendered";
        setReady(true);
        setBusy(false);
        setStatus(
          "完整摘要一致，视频已在隔离播放器加载。请实际审阅画面及声音后明确确认。",
        );
      } else if (v.state === "failed" || v.state === "expired") {
        clearTimeout(timer);
        fail(new ApiError(502, "invalid_response"));
      } else {
        clearTimeout(timer);
        fail(new ApiError(502, "invalid_response"));
      }
    };
    c.signal.addEventListener("abort", () => clearTimeout(timer), {
      once: true,
    });
    // Opaque sandbox origin requires '*'. The port is transferred only to this fixed
    // WindowProxy before untrusted bytes; subsequent communication uses this private port.
    frame.current.contentWindow.postMessage(
      {
        contract: "isolated-mp4-viewer-v1",
        session_id: m.session_id,
        sha256: m.output.sha256,
        byte_length: m.output.byte_length,
        chunk_count: m.output.chunk_count,
        expires_at: m.expires_at,
      },
      "*",
      [channel.port2],
    );
  }
  async function confirm() {
    if (!ready || !manifest || busy || disabled || completed) return;
    setBusy(true);
    setError(null);
    const c = control.current;
    if (!c || c.signal.aborted) return;
    try {
      const r = await request(
        "media-review/complete",
        reviewSchema,
        {
          session_id: manifest.session_id,
          sha256: manifest.output.sha256,
          reviewed: true,
        },
        c.signal,
      );
      if (c.signal.aborted) return;
      if (
        r.approval.id !== approvalId ||
        r.review_id !== manifest.session_id ||
        r.media?.sha256 !== manifest.output.sha256 ||
        r.media.byte_length !== manifest.output.byte_length ||
        r.stage !== "media_output"
      )
        throw new ApiError(502, "invalid_response");
      onComplete(r);
      setStatus("媒体审阅已记录；尚未提交审批决定，请在下方选择意见。");
    } catch (e) {
      if (!c.signal.aborted) fail(e);
    } finally {
      if (!c.signal.aborted) setBusy(false);
    }
  }
  return (
    <section aria-label="隔离媒体审阅">
      <h3>隔离 MP4 审阅</h3>
      <p className="notice">
        内容可能包含风险。技术扫描通过不代表内容安全；最多 64 MiB、5
        分钟审阅窗口。视频只在无登录权限的沙箱解码，不提供外链或下载。
      </p>
      {!url && (
        <p className="notice">
          独立媒体预览站点尚未配置，不能放行媒体；仍可明确拒绝或反馈。
        </p>
      )}
      {url && !manifest && !completed && (
        <button disabled={busy || disabled} onClick={() => void start()}>
          加载隔离视频（读取敏感内容）
        </button>
      )}
      {status && <p role="status">{status}</p>}
      {error !== null && (
        <p role="alert">
          {errorMessage(error)} 媒体决定仍未提交。请关闭并重新审阅。
        </p>
      )}
      {manifest && (
        <>
          <p>
            内容 SHA-256 <code>{manifest.output.sha256}</code> ·{" "}
            {manifest.output.byte_length} 字节 · 审阅截至{" "}
            {time(manifest.expires_at, "Asia/Shanghai")}
          </p>
          <iframe
            ref={frame}
            src={url!}
            title="无登录权限的隔离视频播放器"
            sandbox="allow-scripts"
            referrerPolicy="no-referrer"
            allow="camera 'none'; microphone 'none'; geolocation 'none'; fullscreen 'none'; autoplay 'none'"
            onLoad={loaded}
            className="media-review-frame"
          />
          {!completed && (
            <button
              disabled={!ready || busy || disabled}
              onClick={() => void confirm()}
            >
              我已实际审阅视频，进入审批决定
            </button>
          )}
        </>
      )}
    </section>
  );
}
