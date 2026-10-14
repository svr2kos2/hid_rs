package com.sayodevice.hid_rs

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.hardware.usb.UsbManager
import android.os.Handler
import android.os.Looper
import android.util.Base64
import android.webkit.WebView
import androidx.core.content.ContextCompat
import androidx.webkit.JavaScriptReplyProxy
import androidx.webkit.ScriptHandler
import androidx.webkit.WebMessageCompat
import androidx.webkit.WebViewCompat
import androidx.webkit.WebViewFeature
import org.json.JSONArray
import org.json.JSONObject
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.UUID
import java.util.concurrent.ConcurrentHashMap
import java.util.concurrent.Executors
import java.util.concurrent.ScheduledFuture
import java.util.concurrent.TimeUnit

/**
 * Serves hid_rs running as wasm inside a WebView, which has no WebHID.
 *
 * The page talks to [HOST_OBJECT] through `sayo_hid_bridge_android.js`
 * (injected at document start where supported); that adapter exposes the
 * `__sayoHidBridge` contract consumed by hid_rs' `bridge_hid` backend.
 * All USB work is delegated to [UsbHidBridge].
 *
 * ```
 * val host = WebViewHidHost(context, setOf("https://app.example.com"))
 * host.attach(webView)   // before loadUrl()
 * ...
 * host.detach(webView)
 * ```
 *
 * [allowedOriginRules] follows `WebViewCompat.addWebMessageListener`; only
 * pages from those origins can reach USB devices.
 */
class WebViewHidHost(
    context: Context,
    private val allowedOriginRules: Set<String>,
) {
    companion object {
        const val HOST_OBJECT = "SayoHidBridgeHost"
        private const val TAG = "WebViewHidHost"
        private const val ADAPTER_ASSET = "sayo_hid_bridge_android.js"
        private const val POLL_INTERVAL_MS = 1000L
        private const val DESCRIPTOR_MAX_LEN = 1024
        private const val INPUT_MAX_LEN = 1024

        private const val FRAME_SEND: Byte = 1
        private const val FRAME_SEND_RESULT: Byte = 2
        private const val FRAME_INPUT: Byte = 3
        private const val HEADER_LEN = 21
    }

    private val appContext = context.applicationContext
    private val main = Handler(Looper.getMainLooper())

    // Owns `known` and the poll loop; descriptor reads also run here.
    private val control = Executors.newSingleThreadScheduledExecutor()
    // Serializes output reports so they reach the device in send order.
    private val io = Executors.newSingleThreadExecutor()
    // requestDevices blocks while the permission dialog is up.
    private val permission = Executors.newSingleThreadExecutor()

    // Main thread only.
    private var replyProxy: JavaScriptReplyProxy? = null
    private var scriptHandler: ScriptHandler? = null
    @Volatile private var binary = false

    // Control thread only: devices announced to the page, uuid -> DeviceInfo.
    private val known = LinkedHashMap<String, JSONObject>()
    private var pollTask: ScheduledFuture<*>? = null
    // VID/PID of every device the page has been granted. Android revokes USB
    // permission on unplug, so a returning device is re-prompted, matching
    // WebHID where previously authorized devices reconnect.
    private val grantedVidPids = HashSet<Pair<Int, Int>>()
    private val streaming = ConcurrentHashMap.newKeySet<String>()

    private val detachReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (intent.action == UsbManager.ACTION_USB_DEVICE_DETACHED) {
                control.execute { poll() }
            }
        }
    }
    private var receiverRegistered = false

    /** Installs the message listener (and the adapter script when supported). Call before `loadUrl`. */
    fun attach(webView: WebView): Boolean {
        if (!WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)) {
            FlutterLogger.w(TAG, "WEB_MESSAGE_LISTENER unsupported; WebView HID bridge disabled")
            return false
        }
        WebViewCompat.addWebMessageListener(webView, HOST_OBJECT, allowedOriginRules) { _, message, _, isMainFrame, proxy ->
            if (isMainFrame) onMessage(message, proxy)
        }
        if (WebViewFeature.isFeatureSupported(WebViewFeature.DOCUMENT_START_SCRIPT)) {
            scriptHandler = WebViewCompat.addDocumentStartJavaScript(webView, adapterScript(), allowedOriginRules)
        } else {
            FlutterLogger.w(TAG, "DOCUMENT_START_SCRIPT unsupported; the page must include $ADAPTER_ASSET itself")
        }
        return true
    }

    fun detach(webView: WebView) {
        if (WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_LISTENER)) {
            WebViewCompat.removeWebMessageListener(webView, HOST_OBJECT)
        }
        scriptHandler?.remove()
        scriptHandler = null
        replyProxy = null
        control.execute { shutdown() }
    }

    /** The adapter source, for pages that must include it manually. */
    fun adapterScript(): String =
        appContext.assets.open(ADAPTER_ASSET).bufferedReader().use { it.readText() }

    // ---------------------------------------------------------------- inbound

    private fun onMessage(message: WebMessageCompat, proxy: JavaScriptReplyProxy) {
        replyProxy = proxy
        try {
            if (binary && message.type == WebMessageCompat.TYPE_ARRAY_BUFFER) {
                onFrame(message.arrayBuffer)
                return
            }
            val data = message.data ?: return
            if (data.startsWith("B")) onFrame(Base64.decode(data.substring(1), Base64.NO_WRAP))
            else onJson(JSONObject(data))
        } catch (t: Throwable) {
            FlutterLogger.w(TAG, "dropping malformed bridge message: $t")
        }
    }

    private fun onJson(msg: JSONObject) {
        val id = if (msg.has("id")) msg.getInt("id") else null
        when (val op = msg.optString("op")) {
            "init" -> {
                binary = WebViewFeature.isFeatureSupported(WebViewFeature.WEB_MESSAGE_ARRAY_BUFFER)
                val result = JSONObject().put("binary", binary)
                control.execute {
                    startWatching()
                    reply(id, result)
                }
            }
            "listDevices" -> control.execute {
                poll()
                reply(id, JSONArray(known.values))
            }
            "requestDevices" -> {
                val filters = msg.optJSONArray("filters") ?: JSONArray()
                permission.execute {
                    val infos = JSONArray()
                    try {
                        for (uuid in UsbHidBridge.requestDevices(appContext, filters.toString())) {
                            infos.put(deviceInfo(uuid))
                        }
                        reply(id, infos)
                    } catch (t: Throwable) {
                        replyError(id, "requestDevices failed: $t")
                    }
                    control.execute { poll() }
                }
            }
            "getReportDescriptor" -> {
                val uuid = msg.getString("uuid")
                control.execute {
                    val bytes = UsbHidBridge.getReportDescriptor(appContext, uuid, DESCRIPTOR_MAX_LEN)
                    if (bytes == null) replyError(id, "no report descriptor for $uuid")
                    else reply(id, Base64.encodeToString(bytes, Base64.NO_WRAP))
                }
            }
            "startInput" -> {
                val uuid = msg.getString("uuid")
                streaming.add(uuid)
                control.execute { UsbHidBridge.startInputListener(appContext, uuid, INPUT_MAX_LEN) }
            }
            "stopInput" -> {
                val uuid = msg.getString("uuid")
                streaming.remove(uuid)
                control.execute { UsbHidBridge.stopInputListener(uuid) }
            }
            "shutdown" -> control.execute { shutdown() }
            else -> replyError(id, "unknown op '$op'")
        }
    }

    private fun onFrame(frame: ByteArray) {
        if (frame.size < HEADER_LEN) return
        val buf = ByteBuffer.wrap(frame)
        val type = buf.get()
        val uuid = UUID(buf.long, buf.long).toString()
        val reqId = buf.order(ByteOrder.LITTLE_ENDIAN).int
        val payload = frame.copyOfRange(HEADER_LEN, frame.size)
        if (type != FRAME_SEND) return
        io.execute {
            val written = try {
                UsbHidBridge.sendOutputReport(appContext, uuid, payload)
            } catch (t: Throwable) {
                FlutterLogger.w(TAG, "sendOutputReport($uuid) threw: $t")
                -1
            }
            val result = ByteBuffer.allocate(4).order(ByteOrder.LITTLE_ENDIAN).putInt(written).array()
            postFrame(FRAME_SEND_RESULT, uuid, reqId, result)
        }
    }

    // ----------------------------------------------------------- device watch

    /** Control thread. ATTACHED is not reliably broadcast to receivers, so hot-plug is polled. */
    private fun startWatching() {
        if (pollTask == null) {
            pollTask = control.scheduleWithFixedDelay({ poll() }, 0, POLL_INTERVAL_MS, TimeUnit.MILLISECONDS)
        }
        if (!receiverRegistered) {
            ContextCompat.registerReceiver(
                appContext,
                detachReceiver,
                IntentFilter(UsbManager.ACTION_USB_DEVICE_DETACHED),
                ContextCompat.RECEIVER_NOT_EXPORTED,
            )
            receiverRegistered = true
        }
    }

    /** Control thread. Diffs permitted, openable devices against what the page knows. */
    private fun poll() {
        if (grantedVidPids.isNotEmpty()) {
            try {
                UsbHidBridge.requestPermissionForMatching(appContext, grantedVidPids)
            } catch (t: Throwable) {
                FlutterLogger.w(TAG, "permission re-prompt failed: $t")
            }
        }
        val current = try {
            UsbHidBridge.getDeviceList(appContext).toSet()
        } catch (t: Throwable) {
            FlutterLogger.w(TAG, "device poll failed: $t")
            return
        }
        for (uuid in known.keys.toList()) {
            if (uuid in current) continue
            known.remove(uuid)
            streaming.remove(uuid)
            UsbHidBridge.setInputSink(uuid, null)
            emit(JSONObject().put("event", "deviceRemoved").put("uuid", uuid))
        }
        for (uuid in current) {
            if (uuid in known) continue
            val info = deviceInfo(uuid)
            known[uuid] = info
            grantedVidPids.add(info.getInt("vendorId") to info.getInt("productId"))
            // Installed for every known device so reports from the listener
            // that sendOutputReport auto-starts never pile up in the JNI queue.
            UsbHidBridge.setInputSink(uuid) { data ->
                if (uuid in streaming) postFrame(FRAME_INPUT, uuid, 0, data)
            }
            emit(JSONObject().put("event", "deviceAdded").put("info", info))
        }
    }

    /** Control thread. */
    private fun shutdown() {
        pollTask?.cancel(false)
        pollTask = null
        if (receiverRegistered) {
            try {
                appContext.unregisterReceiver(detachReceiver)
            } catch (_: IllegalArgumentException) {
            }
            receiverRegistered = false
        }
        for (uuid in known.keys) {
            UsbHidBridge.setInputSink(uuid, null)
            if (streaming.remove(uuid)) UsbHidBridge.stopInputListener(uuid)
        }
        known.clear()
        streaming.clear()
    }

    private fun deviceInfo(uuid: String): JSONObject = JSONObject()
        .put("uuid", uuid)
        .put("vendorId", UsbHidBridge.getVid(uuid))
        .put("productId", UsbHidBridge.getPid(uuid))
        .put("productName", UsbHidBridge.getProductName(uuid) ?: JSONObject.NULL)
        .put("serialNumber", UsbHidBridge.getSerialNumber(uuid) ?: JSONObject.NULL)

    // --------------------------------------------------------------- outbound

    private fun reply(id: Int?, result: Any) {
        if (id == null) return
        post(JSONObject().put("id", id).put("ok", true).put("result", result).toString())
    }

    private fun replyError(id: Int?, error: String) {
        FlutterLogger.w(TAG, error)
        if (id == null) return
        post(JSONObject().put("id", id).put("ok", false).put("error", error).toString())
    }

    private fun emit(event: JSONObject) = post(event.toString())

    private fun post(text: String) {
        main.post { replyProxy?.postMessage(text) }
    }

    private fun postFrame(type: Byte, uuid: String, reqId: Int, payload: ByteArray) {
        val id = UUID.fromString(uuid)
        val frame = ByteBuffer.allocate(HEADER_LEN + payload.size)
            .put(type)
            .putLong(id.mostSignificantBits)
            .putLong(id.leastSignificantBits)
            .order(ByteOrder.LITTLE_ENDIAN)
            .putInt(reqId)
            .put(payload)
            .array()
        main.post {
            val proxy = replyProxy ?: return@post
            if (binary) proxy.postMessage(frame)
            else proxy.postMessage("B" + Base64.encodeToString(frame, Base64.NO_WRAP))
        }
    }
}
