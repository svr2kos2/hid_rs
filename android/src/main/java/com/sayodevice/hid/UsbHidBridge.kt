package com.sayodevice.hid

import android.content.Context
import android.app.PendingIntent
import android.content.Intent
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbEndpoint
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbRequest
import android.util.Log

/**
 * Minimal Android-side scaffold for HID over USB. This does NOT yet implement
 * read/write. It exposes a few helpers that can be called via JNI later.
 */
import org.json.JSONArray
import org.json.JSONObject
import java.nio.ByteBuffer
import java.util.Collections
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap

object UsbHidBridge {
    private const val TAG = "UsbHidBridge"
    private const val ACTION_USB_PERMISSION = "com.sayodevice.hid.USB_PERMISSION"
    private val deviceUuidMap = ConcurrentHashMap<String, UsbDevice>()
    private val uuidByDeviceKey = ConcurrentHashMap<String, String>()
    private val deviceKeyByUuid = ConcurrentHashMap<String, String>()
    private val connections = ConcurrentHashMap<String, DeviceConn>()
    // Input listener infrastructure (per device)
    private val inputQueues = ConcurrentHashMap<String, java.util.concurrent.LinkedBlockingQueue<ByteArray>>()
    private val inputThreads = ConcurrentHashMap<String, Thread>()
    private val inputAbort = ConcurrentHashMap<String, java.util.concurrent.atomic.AtomicBoolean>()
    // Track in-flight requests for cooperative cancellation on stop
    private val inputRequests = ConcurrentHashMap<String, MutableList<android.hardware.usb.UsbRequest>>()

    private fun ensureInputListenerRunning(context: Context, uuid: String, hintMaxLen: Int = 0) {
        if (inputThreads.containsKey(uuid)) return
        val dc = ensureConnection(context, uuid) ?: run {
            FlutterLogger.w(TAG, "ensureInputListenerRunning: no connection for $uuid")
            return
        }
        val inEp = dc.inEp ?: run {
            FlutterLogger.w(TAG, "ensureInputListenerRunning: no IN endpoint for $uuid")
            return
        }
        val packetSize = kotlin.math.max(1, inEp.maxPacketSize)
        val desired = if (hintMaxLen > 0) kotlin.math.max(hintMaxLen, packetSize) else packetSize
        FlutterLogger.d(TAG, "ensureInputListenerRunning($uuid) -> starting listener len=$desired")
        startInputListener(context, uuid, desired)
    }

    private fun deviceKey(dev: UsbDevice): String =
        listOf(dev.vendorId, dev.productId, dev.deviceName ?: "", safeSerial(dev)).joinToString(":")

    private fun safeSerial(dev: UsbDevice): String = try {
        dev.serialNumber ?: ""
    } catch (_: SecurityException) {
        ""
    } catch (_: Throwable) {
        ""
    }

    private fun snapshotUsbDevices(context: Context): Pair<UsbManager, List<UsbDevice>> {
        val usb = context.getSystemService(Context.USB_SERVICE) as UsbManager
        val devices = usb.deviceList.values.toList()
        cleanupStaleDevices(devices)
        return usb to devices
    }

    private fun cleanupStaleDevices(currentDevices: Collection<UsbDevice>) {
    val currentKeys = currentDevices.map { deviceKey(it) }.toSet()
        val keysSnapshot = uuidByDeviceKey.keys.toList()
        for (key in keysSnapshot) {
            if (!currentKeys.contains(key)) {
                val uuid = uuidByDeviceKey.remove(key) ?: continue
                deviceKeyByUuid.remove(uuid)
                tearDownDevice(uuid)
            }
        }
    }

    private fun ensureUuidForDevice(dev: UsbDevice): String {
        val key = deviceKey(dev)
        uuidByDeviceKey[key]?.let { existing ->
            deviceUuidMap[existing] = dev
            deviceKeyByUuid[existing] = key
            return existing
        }

        val newUuid = UUID.randomUUID().toString()
        uuidByDeviceKey[key] = newUuid
        deviceKeyByUuid[newUuid] = key
        deviceUuidMap[newUuid] = dev
        return newUuid
    }

    private fun resolveDevice(uuid: String, usb: UsbManager, currentDevices: List<UsbDevice>): UsbDevice? {
        val key = deviceKeyByUuid[uuid] ?: return null
        val found = currentDevices.firstOrNull { deviceKey(it) == key } ?: return null
        if (uuidByDeviceKey[key] != uuid) {
            return null
        }
        deviceUuidMap[uuid] = found
        return found
    }

    private fun tearDownDevice(uuid: String) {
        stopInputListener(uuid)
        inputThreads.remove(uuid)?.interrupt()
        inputAbort.remove(uuid)
        inputQueues.remove(uuid)?.clear()
        inputRequests.remove(uuid)?.forEach { request ->
            try {
                request.close()
            } catch (_: Throwable) {
            }
        }
        connections.remove(uuid)?.let { conn ->
            try {
                conn.conn.releaseInterface(conn.iface)
            } catch (_: Throwable) {
            }
            try {
                conn.conn.close()
            } catch (_: Throwable) {
            }
        }
        deviceUuidMap.remove(uuid)
        deviceKeyByUuid.remove(uuid)
    }

    @JvmStatic
    fun isSupported(context: Context): Boolean {
        val hasUsb = context.packageManager.hasSystemFeature("android.hardware.usb.host")
        FlutterLogger.i(TAG, "isSupported -> USB host supported = $hasUsb")
        return hasUsb
    }

    @JvmStatic
    fun listDevices(context: Context): List<Map<String, Any>> {
    val usb = context.getSystemService(Context.USB_SERVICE) as UsbManager
        val res = mutableListOf<Map<String, Any>>()
        for (entry in usb.deviceList.values) {
            val dev = entry
            res.add(
                mapOf(
                    "vendorId" to dev.vendorId,
                    "productId" to dev.productId,
                    "deviceName" to (dev.deviceName ?: ""),
                    "productName" to (tryGetProductName(dev) ?: "")
                )
            )
        }
        FlutterLogger.d(TAG, "listDevices -> Found ${res.size} USB devices")
        return res
    }

    @JvmStatic
    fun requestDevices(context: Context, filtersJson: String): Array<String> {
        // Align semantics with Web: prompt for permission and return only the first
        // device the user grants permission to (from the filtered set). This call
        // blocks (polling) for a short time to wait for permission result.
        val (usb, devices) = snapshotUsbDevices(context)
        val filters = parseFilters(filtersJson)
        // Pick the first matching device
        val candidate: UsbDevice? = devices.firstOrNull { dev ->
            filters.isEmpty() || filters.any { (vid, pid) ->
                (vid == null || vid == dev.vendorId) && (pid == null || pid == dev.productId)
            }
        }
        if (candidate == null) {
            FlutterLogger.i(TAG, "requestDevices -> no candidate matched filters: $filters")
            return emptyArray()
        }

        val uuid = ensureUuidForDevice(candidate)

        // If already permitted, ensure connection and return
        if (usb.hasPermission(candidate)) {
            FlutterLogger.i(TAG, "requestDevices -> already has permission, ensuring connection")
            return if (ensureConnection(context, uuid) != null) arrayOf(uuid) else emptyArray()
        }

        // Request permission and poll up to ~15 seconds for result
        // Android 14+ forbids creating mutable implicit PendingIntents; use explicit and immutable.
        val appCtx = context.applicationContext
        val intent = Intent(appCtx, UsbPermissionReceiver::class.java).apply {
            action = ACTION_USB_PERMISSION
        }
        val piFlags = if (android.os.Build.VERSION.SDK_INT >= 23) PendingIntent.FLAG_IMMUTABLE else 0
        val pi = PendingIntent.getBroadcast(appCtx, 0, intent, piFlags)
        return try {
            FlutterLogger.i(TAG, "requestDevices -> requesting permission for ${candidate.deviceName} vid=${candidate.vendorId} pid=${candidate.productId}")
            usb.requestPermission(candidate, pi)
            var waitedMs = 0
            val stepMs = 100L
            val maxWaitMs = 15_000
            while (waitedMs < maxWaitMs) {
                if (usb.hasPermission(candidate)) {
                    FlutterLogger.i(TAG, "requestDevices -> permission granted after ${waitedMs}ms; ensuring connection")
                    return if (ensureConnection(context, uuid) != null) arrayOf(uuid) else emptyArray()
                }
                try { Thread.sleep(stepMs) } catch (_: InterruptedException) { /* ignore */ }
                waitedMs += stepMs.toInt()
            }
            val name = candidate.deviceName ?: "unknown"
            FlutterLogger.w(TAG, "requestDevices -> Permission not granted within timeout for $name")
            emptyArray()
        } catch (t: Throwable) {
            FlutterLogger.w(TAG, "requestDevices -> requestPermission threw: $t")
            emptyArray()
        }
    }

    @JvmStatic
    fun requestDevicesJson(context: Context, filtersJson: String): String {
        return JSONArray(requestDevices(context, filtersJson).toList()).toString()
    }

    @JvmStatic
    fun getDeviceList(context: Context): Array<String> {
        // Only return devices that already have permission and can be opened/claimed
        val (usb, devices) = snapshotUsbDevices(context)
        val uuids = mutableListOf<String>()
        for (dev in devices) {
            if (!usb.hasPermission(dev)) continue
            val uuid = ensureUuidForDevice(dev)
            // ensureConnection will cache a connection on success
            if (ensureConnection(context, uuid) != null) {
                uuids.add(uuid)
            }
        }
        FlutterLogger.d(TAG, "getDeviceList -> returning ${uuids.size} ready devices")
        return uuids.toTypedArray()
    }

    @JvmStatic
    fun getDeviceListJson(context: Context): String {
        return JSONArray(getDeviceList(context).toList()).toString()
    }

    @JvmStatic
    fun getVid(uuid: String): Int = deviceUuidMap[uuid]?.vendorId ?: -1

    @JvmStatic
    fun getPid(uuid: String): Int = deviceUuidMap[uuid]?.productId ?: -1

    @JvmStatic
    fun getProductName(uuid: String): String? {
        val dev = deviceUuidMap[uuid] ?: return null
        return tryGetProductName(dev)
    }

    data class DeviceConn(
        val device: UsbDevice,
        val iface: UsbInterface,
        val conn: UsbDeviceConnection,
        val inEp: UsbEndpoint?,
        val outEp: UsbEndpoint?
    )

    @JvmStatic
    fun hasPermission(context: Context, uuid: String): Boolean {
        val (usb, devices) = snapshotUsbDevices(context)
        val dev = resolveDevice(uuid, usb, devices) ?: return false
        return usb.hasPermission(dev)
    }

    @JvmStatic
    fun requestPermission(context: Context, uuid: String): Boolean {
        val (usb, devices) = snapshotUsbDevices(context)
        val dev = resolveDevice(uuid, usb, devices) ?: return false
        if (usb.hasPermission(dev)) return true
        // Use explicit, immutable PendingIntent to satisfy Android 14+ restrictions
        val appCtx = context.applicationContext
        val intent = Intent(appCtx, UsbPermissionReceiver::class.java).apply {
            action = ACTION_USB_PERMISSION
        }
        val flags = if (android.os.Build.VERSION.SDK_INT >= 23) PendingIntent.FLAG_IMMUTABLE else 0
        val pi = PendingIntent.getBroadcast(appCtx, 0, intent, flags)
        return try {
            FlutterLogger.i(TAG, "requestPermission(uuid=$uuid) -> requesting permission")
            usb.requestPermission(dev, pi)
            false // permission result async; caller should poll via hasPermission/getDeviceList
        } catch (t: Throwable) {
            FlutterLogger.w(TAG, "requestPermission(uuid=$uuid) failed: $t")
            false
        }
    }

    @JvmStatic
    fun available(context: Context, uuid: String): Boolean {
        return try {
            val ok = ensureConnection(context, uuid) != null
            FlutterLogger.d(TAG, "available(uuid=$uuid) -> $ok")
            ok
        } catch (t: Throwable) {
            FlutterLogger.w(TAG, "available(uuid=$uuid) failed: $t")
            false
        }
    }

    @JvmStatic
    fun sendOutputReport(context: Context, uuid: String, data: ByteArray): Int {
        val dc = ensureConnection(context, uuid) ?: run { FlutterLogger.w(TAG, "sendOutputReport: no connection for $uuid"); return -1 }
        ensureInputListenerRunning(context, uuid)
        FlutterLogger.d(TAG, "sendOutputReport(uuid=$uuid) -> len=${data.size} outEp=${dc.outEp != null}")
        FlutterLogger.v(TAG, "sendOutputReport(uuid=$uuid) data=${toHex(data, 64)}")
        // Prefer interrupt/bulk OUT endpoint
        dc.outEp?.let { out ->
            val written = dc.conn.bulkTransfer(out, data, data.size, 1000)
            FlutterLogger.d(TAG, "sendOutputReport(uuid=$uuid) bulkTransfer -> ${written ?: -1}")
            return written ?: -1
        }
        // Fallback to HID Set_Report via control transfer (class, interface)
        // bmRequestType: 0x21 (Host to device | Class | Interface)
        // bRequest: 0x09 (SET_REPORT)
        // wValue: (ReportType << 8) | ReportID; we assume Output(2) and ReportID 0
        val reqType = 0x21
        val request = 0x09
        val wValue = (2 shl 8) or 0
        val wIndex = dc.iface.id
        val sent = dc.conn.controlTransfer(reqType, request, wValue, wIndex, data, data.size, 1000)
        FlutterLogger.d(TAG, "sendOutputReport(uuid=$uuid) controlTransfer -> $sent")
        return sent
    }

    @JvmStatic
    fun readInputReport(context: Context, uuid: String, maxLen: Int, timeoutMs: Int): ByteArray? {
        val dc = ensureConnection(context, uuid) ?: run { FlutterLogger.w(TAG, "readInputReport: no connection for $uuid"); return null }
        val ep = dc.inEp ?: return null
        val buf = ByteArray(maxLen)
        // FlutterLogger.d(TAG, "readInputReport: bulkTransfer IN maxLen=$maxLen timeout=$timeoutMs")
        val n = dc.conn.bulkTransfer(ep, buf, maxLen, timeoutMs)
        return if (n != null && n > 0) buf.copyOf(n) else null
    }

    // Start a background listener that blocks on UsbRequest.requestWait and pushes bytes into a queue
    @JvmStatic
    fun startInputListener(context: Context, uuid: String, maxLen: Int): Boolean {
        val dc = ensureConnection(context, uuid) ?: run { FlutterLogger.w(TAG, "startInputListener: no connection for $uuid"); return false }
        val inEp = dc.inEp ?: run { FlutterLogger.w(TAG, "startInputListener: no IN endpoint for $uuid"); return false }
        if (inputThreads.containsKey(uuid)) { FlutterLogger.d(TAG, "startInputListener: already running for $uuid"); return true }

        val queue = inputQueues.getOrPut(uuid) { java.util.concurrent.LinkedBlockingQueue() }
        val abort = inputAbort.getOrPut(uuid) { java.util.concurrent.atomic.AtomicBoolean(false) }
        abort.set(false)

        val t = Thread({
            android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_BACKGROUND)
            val ep = inEp
            // Prefer UsbRequest queueing for async reads; fall back to bulkTransfer if request setup fails
            val useRequests = try {
                val list = inputRequests.getOrPut(uuid) { Collections.synchronizedList(mutableListOf()) }
                synchronized(list) { list.clear() }
                true
            } catch (_: Throwable) {
                inputRequests.remove(uuid)
                FlutterLogger.w(TAG, "startInputListener($uuid) -> UsbRequest setup failed, falling back to bulk")
                false
            }
            FlutterLogger.d(TAG, "startInputListener($uuid) -> epType=${ep.type} useRequests=$useRequests maxLen=$maxLen")
            if (useRequests) {
                val requests = inputRequests[uuid]!!
                val packetSize = kotlin.math.max(1, ep.maxPacketSize)
                val readLen = if (maxLen > 0) kotlin.math.min(maxLen, packetSize) else packetSize
                FlutterLogger.d(TAG, "startInputListener($uuid) request loop readLen=$readLen")
                try {
                    while (!abort.get()) {
                        val req = UsbRequest()
                        if (!req.initialize(dc.conn, ep)) {
                            FlutterLogger.w(TAG, "startInputListener($uuid) -> UsbRequest initialize failed")
                            req.close()
                            Thread.sleep(5)
                            continue
                        }
                        val buffer = ByteBuffer.allocate(readLen)
                        requests.add(req)
                        if (!req.queue(buffer, readLen)) {
                            FlutterLogger.w(TAG, "startInputListener($uuid) -> queue failed")
                            requests.remove(req)
                            req.close()
                            Thread.sleep(5)
                            continue
                        }
                        val completed = dc.conn.requestWait()
                        requests.remove(req)
                        val len = buffer.position()
                        if (completed == null) {
                            FlutterLogger.w(TAG, "startInputListener($uuid) -> requestWait returned null")
                        }
                        if (completed == req && len > 0) {
                            val arr = ByteArray(len)
                            buffer.flip()
                            buffer.get(arr)
                            val offered = queue.offer(arr)
                            FlutterLogger.d(TAG, "startInputListener($uuid) -> queued ${arr.size} bytes via UsbRequest offered=$offered data=${toHex(arr, 128)}")
                            if (!offered) {
                                FlutterLogger.w(TAG, "startInputListener($uuid) -> queue.offer returned false (capacity reached?)")
                            }
                        } else if (len == 0) {
                            FlutterLogger.d(TAG, "startInputListener($uuid) -> zero-length packet via UsbRequest")
                        }
                        req.close()
                    }
                } catch (t: Throwable) {
                    FlutterLogger.w(TAG, "startInputListener request loop error for $uuid: $t")
                } finally {
                    synchronized(requests) {
                        requests.forEach {
                            try { it.cancel() } catch (_: Throwable) {}
                            try { it.close() } catch (_: Throwable) {}
                        }
                        requests.clear()
                    }
                    inputThreads.remove(uuid)
                }
            } else {
                // Read in a tight loop using bulkTransfer to obtain the exact transferred length
                val packetSize = kotlin.math.max(1, ep.maxPacketSize)
                val readLen = if (maxLen > 0) kotlin.math.min(maxLen, packetSize) else packetSize
                val arr = ByteArray(readLen)
                FlutterLogger.d(TAG, "startInputListener($uuid) bulk loop readLen=$readLen")
                try {
                    while (!abort.get()) {
                        val n = dc.conn.bulkTransfer(ep, arr, readLen, 2000)
                        if (n == null) {
                            FlutterLogger.v(TAG, "startInputListener($uuid) bulkTransfer returned null")
                            Thread.sleep(5)
                            continue
                        }
                        if (n > 0) {
                            val copy = arr.copyOf(n)
                            val offered = queue.offer(copy)
                            FlutterLogger.d(TAG, "startInputListener($uuid) -> queued $n bytes via bulkTransfer offered=$offered data=${toHex(copy, 128)}")
                            if (!offered) {
                                FlutterLogger.w(TAG, "startInputListener($uuid) -> bulk queue.offer returned false (capacity reached?)")
                            }
                        } else {
                            FlutterLogger.d(TAG, "startInputListener($uuid) -> zero-length packet via bulkTransfer")
                        }
                    }
                } catch (t: Throwable) {
                    FlutterLogger.w(TAG, "startInputListener bulk loop error for $uuid: $t")
                } finally {
                    inputThreads.remove(uuid)
                }
            }
            FlutterLogger.d(TAG, "startInputListener($uuid) thread exiting")
        }, "UsbInputListener-$uuid")
        inputThreads[uuid] = t
        t.start()
        return true
    }

    @JvmStatic
    fun stopInputListener(uuid: String) {
        inputAbort[uuid]?.set(true)
        FlutterLogger.d(TAG, "stopInputListener($uuid)")
        // Best-effort: actively cancel any in-flight requests to unblock requestWait
        inputRequests[uuid]?.let { list ->
            synchronized(list) {
                list.forEach { r ->
                    try { r.cancel() } catch (_: Throwable) {}
                }
            }
        }
        // The worker thread will close and clean up
    }

    @JvmStatic
    fun takeInputReport(context: Context, uuid: String, timeoutMs: Int): ByteArray? {
        ensureConnection(context, uuid) ?: return null
        ensureInputListenerRunning(context, uuid)
        val q = inputQueues[uuid] ?: return null
        return try {
            val sizeBefore = q.size
            FlutterLogger.d(TAG, "takeInputReport($uuid) waiting timeout=$timeoutMs queueSize=$sizeBefore")
            val result = if (timeoutMs <= 0) q.take() else q.poll(timeoutMs.toLong(), java.util.concurrent.TimeUnit.MILLISECONDS)
            if (result != null) {
                FlutterLogger.d(TAG, "takeInputReport($uuid) -> len=${result.size}")
            } else {
                FlutterLogger.d(TAG, "takeInputReport($uuid) -> timeout after ${timeoutMs}ms")
            }
            result
        } catch (_: InterruptedException) {
            null
        }
    }

    @JvmStatic
    fun getReportDescriptor(context: Context, uuid: String, maxLen: Int): ByteArray? {
        val dc = ensureConnection(context, uuid) ?: run { FlutterLogger.w(TAG, "getReportDescriptor: no connection for $uuid"); return null }
        val iface = dc.iface
        val buf = ByteArray(maxLen)
        // Standard GET_DESCRIPTOR request for HID Report Descriptor at the interface level
        val bmRequestType = 0x81 // Device-to-host | Standard | Interface
        val bRequest = 0x06 // GET_DESCRIPTOR
        val wValue = (0x22 shl 8) or 0 // REPORT descriptor type
        val wIndex = iface.id
        FlutterLogger.d(TAG, "getReportDescriptor: controlTransfer GET_DESCRIPTOR len=$maxLen if=${iface.id}")
        val n = dc.conn.controlTransfer(bmRequestType, bRequest, wValue, wIndex, buf, maxLen, 1000)
        return if (n != null && n > 0) buf.copyOf(n) else null
    }

    private fun ensureConnection(context: Context, uuid: String): DeviceConn? {
        val (usb, devices) = snapshotUsbDevices(context)
        val dev = resolveDevice(uuid, usb, devices) ?: return null

        connections[uuid]?.let { existing ->
            val sameDevice = try {
                deviceKey(existing.device) == deviceKey(dev)
            } catch (_: Throwable) {
                false
            }
            if (sameDevice && usb.hasPermission(dev)) {
                deviceUuidMap[uuid] = dev
                FlutterLogger.d(TAG, "ensureConnection($uuid) -> cached")
                return existing
            }
            connections.remove(uuid)
            try {
                existing.conn.releaseInterface(existing.iface)
            } catch (_: Throwable) {
            }
            try {
                existing.conn.close()
            } catch (_: Throwable) {
            }
        }

        if (!usb.hasPermission(dev)) return null
        FlutterLogger.d(TAG, "ensureConnection($uuid) -> opening device ${dev.deviceName}")
        val conn = usb.openDevice(dev) ?: run { FlutterLogger.w(TAG, "ensureConnection($uuid) -> openDevice returned null"); return null }
        // Choose the best HID interface. Prefer:
        // 1) An interface whose report descriptor contains Report IDs 0x02/0x21/0x22
        // 2) Any interface that contains any Report ID (0x85)
        // 3) A non-boot HID interface (subclass 0, protocol 0)
        // 4) Otherwise, the first HID interface
        var bestIface: UsbInterface? = null
        var hasSpecificIds = false
        var hasAnyIds = false
        var nonBootCandidate: UsbInterface? = null

        fun tryReadReportDescriptor(tmpConn: UsbDeviceConnection, intf: UsbInterface, maxLen: Int = 1024): ByteArray? {
            val buf = ByteArray(maxLen)
            val bmRequestType = 0x81 // Device-to-host | Standard | Interface
            val bRequest = 0x06 // GET_DESCRIPTOR
            val wValue = (0x22 shl 8) or 0 // REPORT descriptor type
            val wIndex = intf.id
            val n = tmpConn.controlTransfer(bmRequestType, bRequest, wValue, wIndex, buf, maxLen, 200)
            return if (n != null && n > 0) buf.copyOf(n) else null
        }

        for (i in 0 until dev.interfaceCount) {
            val intf = dev.getInterface(i)
            if (intf.interfaceClass != 3 /* HID */) continue

            // Track a non-boot HID as a fallback candidate
            if (intf.interfaceSubclass == 0 && intf.interfaceProtocol == 0 && nonBootCandidate == null) {
                nonBootCandidate = intf
            }

            // Probe this interface's report descriptor
            val claimed = conn.claimInterface(intf, true)
            if (!claimed) {
                FlutterLogger.w(TAG, "ensureConnection($uuid) -> could not claim iface idx=$i class=3 sub=${intf.interfaceSubclass} proto=${intf.interfaceProtocol}")
                continue
            }
            val desc = tryReadReportDescriptor(conn, intf)
            // Release immediately if not chosen later
            conn.releaseInterface(intf)

            if (desc != null) {
                val has85 = desc.contains(0x85.toByte())
                val matchSpecific = run {
                    var found = false
                    var j = 0
                    while (j < desc.size - 1) {
                        if (desc[j] == 0x85.toByte()) {
                            val id = desc[j + 1]
                            if (id == 0x02.toByte() || id == 0x21.toByte() || id == 0x22.toByte()) { found = true; break }
                            j += 2; continue
                        }
                        j += 1
                    }
                    found
                }
                FlutterLogger.d(TAG, "Iface idx=$i sub=${intf.interfaceSubclass} proto=${intf.interfaceProtocol} reportLen=${desc.size} has85=$has85 matchSpecific=$matchSpecific")
                if (matchSpecific) {
                    bestIface = intf
                    hasSpecificIds = true
                    break // strongest match
                } else if (has85 && !hasSpecificIds && !hasAnyIds) {
                    bestIface = intf
                    hasAnyIds = true
                } else if (!hasSpecificIds && !hasAnyIds && nonBootCandidate == intf && bestIface == null) {
                    bestIface = intf
                } else if (bestIface == null) {
                    bestIface = intf // fallback to first HID seen
                }
            } else {
                // No descriptor read; still consider non-boot as a fallback
                if (!hasSpecificIds && !hasAnyIds && nonBootCandidate == intf && bestIface == null) {
                    bestIface = intf
                } else if (bestIface == null) {
                    bestIface = intf
                }
            }
        }

        if (bestIface == null && dev.interfaceCount > 0) {
            bestIface = dev.getInterface(0)
        }
        val iface = bestIface ?: run { conn.close(); return null }
        if (!conn.claimInterface(iface, true)) {
            FlutterLogger.w(TAG, "ensureConnection($uuid) -> final claimInterface failed for chosen iface")
            conn.close(); return null
        }
        var inEp: UsbEndpoint? = null
        var outEp: UsbEndpoint? = null
        for (e in 0 until iface.endpointCount) {
            val ep = iface.getEndpoint(e)
            val dirIn = ep.direction == UsbConstants.USB_DIR_IN
            val dirOut = ep.direction == UsbConstants.USB_DIR_OUT
            if (dirIn && inEp == null) inEp = ep
            if (dirOut && outEp == null) outEp = ep
        }
        val dc = DeviceConn(dev, iface, conn, inEp, outEp)
        FlutterLogger.d(TAG, "ensureConnection($uuid) -> chosen iface sub=${iface.interfaceSubclass} proto=${iface.interfaceProtocol} inEp=${inEp!=null} outEp=${outEp!=null}")
        connections[uuid] = dc
        return dc
    }

    private fun tryGetProductName(dev: UsbDevice): String? {
        // Product name typically needs opening an interface; keep simple for scaffold
        return null
    }

    private fun parseFilters(json: String): List<Pair<Int?, Int?>> {
        return try {
            val arr = JSONArray(json)
            (0 until arr.length()).map { idx ->
                val obj = arr.getJSONObject(idx)
                val vid = if (obj.has("vendorId") && !obj.isNull("vendorId")) obj.getInt("vendorId") else null
                val pid = if (obj.has("productId") && !obj.isNull("productId")) obj.getInt("productId") else null
                vid to pid
            }
        } catch (t: Throwable) {
            Log.w(TAG, "Failed to parse filters: $t")
            emptyList()
        }
    }

    private fun toHex(bytes: ByteArray, maxShow: Int = 256): String {
        if (bytes.isEmpty()) return ""
        val sb = StringBuilder()
        val limit = kotlin.math.min(bytes.size, maxShow)
        for (i in 0 until limit) {
            sb.append(String.format("%02X", bytes[i]))
            if (i != limit - 1) sb.append(' ')
        }
        if (bytes.size > limit) sb.append(" …(+").append(bytes.size - limit).append(")")
        return sb.toString()
    }
}
