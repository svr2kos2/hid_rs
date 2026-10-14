// In-memory implementation of the `__sayoHidBridge` contract consumed by
// `src/bridge_hid.rs`. Used by the wasm tests and for browser-side
// development without an Android device.
//
//   const fake = createFakeSayoHidBridge();
//   fake.install();                       // sets globalThis.__sayoHidBridge
//   fake.connect({ uuid, vendorId, productId, productName, serialNumber,
//                  descriptor: Uint8Array,
//                  onOutputReport(data, emitInput) { ... } });
//   fake.disconnect(uuid);
//   fake.emitInput(uuid, Uint8Array);
//
// A device added with `granted: false` is only returned by
// `requestDevices`, mirroring Android's permission flow; with
// `announceOnGrant: true` granting it also fires `onDeviceAdded`.

export function createFakeSayoHidBridge() {
  const devices = new Map();
  const streaming = new Set();
  const calls = [];
  let handler = null;

  const info = (dev) => ({
    uuid: dev.uuid,
    vendorId: dev.vendorId,
    productId: dev.productId,
    productName: dev.productName ?? null,
    serialNumber: dev.serialNumber ?? null,
  });

  const matches = (dev, filters) =>
    filters.length === 0 ||
    filters.some(
      (f) =>
        (f.vendorId == null || f.vendorId === dev.vendorId) &&
        (f.productId == null || f.productId === dev.productId),
    );

  const lookup = (uuid) => {
    const dev = devices.get(uuid);
    if (!dev || !dev.granted) throw new Error(`unknown device ${uuid}`);
    return dev;
  };

  const emitInput = (uuid, data) => {
    if (!streaming.has(uuid) || !handler) return false;
    handler.onInputReport(uuid, new Uint8Array(data));
    return true;
  };

  const bridge = {
    async init() {
      calls.push(["init"]);
    },
    async listDevices() {
      calls.push(["listDevices"]);
      return [...devices.values()].filter((d) => d.granted).map(info);
    },
    async requestDevices(filters) {
      calls.push(["requestDevices", filters]);
      const candidate = [...devices.values()].find((d) => matches(d, filters));
      if (!candidate) return [];
      candidate.granted = true;
      // Like the Android host, whose device poll can announce the device
      // before the requestDevices reply arrives.
      if (candidate.announceOnGrant && handler) handler.onDeviceAdded(info(candidate));
      return [info(candidate)];
    },
    async getReportDescriptor(uuid) {
      calls.push(["getReportDescriptor", uuid]);
      return new Uint8Array(lookup(uuid).descriptor);
    },
    async sendReport(uuid, data) {
      calls.push(["sendReport", uuid, Array.from(data)]);
      const dev = lookup(uuid);
      dev.onOutputReport?.(new Uint8Array(data), (report) => {
        // Deliver asynchronously, like a real device round-trip.
        queueMicrotask(() => emitInput(uuid, report));
      });
      return data.length;
    },
    startInput(uuid) {
      calls.push(["startInput", uuid]);
      streaming.add(uuid);
    },
    stopInput(uuid) {
      calls.push(["stopInput", uuid]);
      streaming.delete(uuid);
    },
    shutdown() {
      calls.push(["shutdown"]);
      streaming.clear();
    },
    setEventHandler(h) {
      handler = h;
    },
  };

  return {
    bridge,
    calls,
    install() {
      globalThis.__sayoHidBridge = bridge;
    },
    uninstall() {
      if (globalThis.__sayoHidBridge === bridge) delete globalThis.__sayoHidBridge;
    },
    connect(dev) {
      const entry = { granted: true, ...dev };
      devices.set(dev.uuid, entry);
      if (entry.granted && handler) handler.onDeviceAdded(info(entry));
    },
    disconnect(uuid) {
      const dev = devices.get(uuid);
      devices.delete(uuid);
      streaming.delete(uuid);
      if (dev?.granted && handler) handler.onDeviceRemoved(uuid);
    },
    emitInput,
    isStreaming: (uuid) => streaming.has(uuid),
  };
}
