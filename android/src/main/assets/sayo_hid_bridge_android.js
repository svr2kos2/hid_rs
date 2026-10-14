// Adapts the `SayoHidBridgeHost` WebMessageListener object injected by
// `WebViewHidHost.kt` to the `globalThis.__sayoHidBridge` contract consumed
// by hid_rs (`src/bridge_hid.rs`). Injected at document start by the host;
// pages may also include it manually on WebViews without that feature.
//
// Wire protocol:
// - Control messages are JSON strings. Requests carry `id`; replies are
//   `{id, ok, result}` / `{id, ok: false, error}`; events carry `event`.
// - Report traffic uses binary frames:
//     [type u8][uuid 16 B][reqId u32 LE][payload]
//     1 = sendReport (page -> host), 2 = sendReport result (host -> page,
//     payload i32 LE bytes written, negative on failure), 3 = input report
//     (host -> page, reqId 0).
//   Frames travel as ArrayBuffer when the host reports `binary: true` from
//   `init`, otherwise as the string "B" + base64(frame).
(function () {
  "use strict";
  if (globalThis.__sayoHidBridge) return;
  const host = globalThis.SayoHidBridgeHost;
  if (!host) return;

  const FRAME_SEND = 1;
  const FRAME_SEND_RESULT = 2;
  const FRAME_INPUT = 3;
  const HEADER_LEN = 21;

  const pending = new Map();
  let nextId = 1;
  let binary = false;
  let handler = null;

  const uuidToBytes = (uuid) => {
    const hex = uuid.replace(/-/g, "");
    if (hex.length !== 32) throw new Error(`invalid device uuid ${uuid}`);
    const out = new Uint8Array(16);
    for (let i = 0; i < 16; i++) out[i] = parseInt(hex.substr(i * 2, 2), 16);
    return out;
  };

  const bytesToUuid = (bytes, offset) => {
    let hex = "";
    for (let i = 0; i < 16; i++) hex += bytes[offset + i].toString(16).padStart(2, "0");
    return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
  };

  const toBase64 = (bytes) => {
    let s = "";
    for (let i = 0; i < bytes.length; i += 0x8000) {
      s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
    }
    return btoa(s);
  };

  const fromBase64 = (text) => Uint8Array.from(atob(text), (c) => c.charCodeAt(0));

  const settle = (id, ok, value) => {
    const entry = pending.get(id);
    if (!entry) return;
    pending.delete(id);
    if (ok) entry.resolve(value);
    else entry.reject(new Error(String(value)));
  };

  const request = (op, fields) =>
    new Promise((resolve, reject) => {
      const id = nextId++;
      pending.set(id, { resolve, reject });
      host.postMessage(JSON.stringify({ id, op, ...fields }));
    });

  const notify = (op, fields) => host.postMessage(JSON.stringify({ op, ...fields }));

  const sendFrame = (type, uuid, reqId, payload) => {
    const frame = new Uint8Array(HEADER_LEN + payload.length);
    frame[0] = type;
    frame.set(uuidToBytes(uuid), 1);
    new DataView(frame.buffer).setUint32(17, reqId, true);
    frame.set(payload, HEADER_LEN);
    host.postMessage(binary ? frame.buffer : "B" + toBase64(frame));
  };

  const onFrame = (frame) => {
    if (frame.length < HEADER_LEN) return;
    const uuid = bytesToUuid(frame, 1);
    const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength);
    const reqId = view.getUint32(17, true);
    const payload = frame.subarray(HEADER_LEN);
    switch (frame[0]) {
      case FRAME_SEND_RESULT:
        settle(reqId, true, payload.length >= 4 ? view.getInt32(HEADER_LEN, true) : -1);
        break;
      case FRAME_INPUT:
        handler?.onInputReport(uuid, payload);
        break;
    }
  };

  const onJson = (msg) => {
    if (msg.event === "deviceAdded") handler?.onDeviceAdded(msg.info);
    else if (msg.event === "deviceRemoved") handler?.onDeviceRemoved(msg.uuid);
    else if (msg.id != null) settle(msg.id, msg.ok, msg.ok ? msg.result : msg.error);
  };

  const onMessage = (event) => {
    const data = event.data;
    if (typeof data !== "string") onFrame(new Uint8Array(data));
    else if (data[0] === "B") onFrame(fromBase64(data.slice(1)));
    else onJson(JSON.parse(data));
  };
  if (typeof host.addEventListener === "function") host.addEventListener("message", onMessage);
  else host.onmessage = onMessage;

  globalThis.__sayoHidBridge = {
    async init() {
      const result = await request("init", {});
      binary = !!result?.binary;
    },
    listDevices: () => request("listDevices", {}),
    requestDevices: (filters) => request("requestDevices", { filters }),
    async getReportDescriptor(uuid) {
      return fromBase64(await request("getReportDescriptor", { uuid }));
    },
    sendReport: (uuid, data) =>
      new Promise((resolve, reject) => {
        const id = nextId++;
        pending.set(id, { resolve, reject });
        sendFrame(FRAME_SEND, uuid, id, data);
      }),
    startInput: (uuid) => notify("startInput", { uuid }),
    stopInput: (uuid) => notify("stopInput", { uuid }),
    shutdown() {
      notify("shutdown", {});
      for (const [, entry] of pending) entry.reject(new Error("bridge shut down"));
      pending.clear();
    },
    setEventHandler(h) {
      handler = h;
    },
  };
})();
