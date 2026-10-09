package com.iameberhard.vectorcraft

import android.content.ClipboardManager
import android.content.ContentValues
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.os.CancellationSignal
import android.os.Environment
import android.os.ParcelFileDescriptor
import android.print.PageRange
import android.print.PrintAttributes
import android.print.PrintDocumentAdapter
import android.print.PrintDocumentInfo
import android.print.PrintManager
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.util.Log
import android.widget.Toast
import androidx.activity.result.contract.ActivityResultContracts
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import com.google.androidgamesdk.GameActivity
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

/**
 * The Android shell around the Rust app (`apps/vectorcraft-android`, loaded as
 * `libvectorcraft_android.so` by GameActivity's glue). The Rust side calls [pickOpen], [pickPlace],
 * [saveToDownloads], [openUrl], [readClipboard], [printPdf] and [localeTags] through JNI; picked
 * files go back through [nativeDeliverFile] (File › Open) and [nativeDeliverPlace] (File › Place).
 */
class MainActivity : GameActivity() {

    companion object {
        private const val TAG = "vectorcraft"
        private const val FOLDER = "VectorCraft"

        init {
            System.loadLibrary("vectorcraft_android")
        }
    }

    /** Implemented in Rust: hands a picked file (name, contents) to the app, to open as a document. */
    private external fun nativeDeliverFile(name: String, bytes: ByteArray)

    /** Implemented in Rust: hands a picked file (name, contents) to the app, to place in the document. */
    private external fun nativeDeliverPlace(name: String, bytes: ByteArray)

    /** Files this session created in Downloads, so saving the same name again overwrites. */
    private val savedUris = HashMap<String, Uri>()

    private val openDocument = registerForActivityResult(ActivityResultContracts.OpenDocument()) { uri: Uri? ->
        if (uri == null) return@registerForActivityResult
        readInBackground(uri) { name, bytes -> nativeDeliverFile(name, bytes) }
    }

    private val placeDocuments = registerForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) { uris: List<Uri> ->
        for (uri in uris) readInBackground(uri) { name, bytes -> nativeDeliverPlace(name, bytes) }
    }

    /** Read [uri] off the UI thread (a large PDF can take a while) and hand it to [deliver]. */
    private fun readInBackground(uri: Uri, deliver: (String, ByteArray) -> Unit) {
        Thread {
            try {
                val name = displayName(uri) ?: "document"
                val bytes = contentResolver.openInputStream(uri)?.use { it.readBytes() }
                if (bytes == null) {
                    toast("Couldn't read $name")
                } else {
                    deliver(name, bytes)
                }
            } catch (e: Exception) {
                Log.e(TAG, "open failed", e)
                toast("Couldn't open the file: ${e.message}")
            }
        }.start()
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        hideSystemBars()
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus) hideSystemBars()
    }

    /** Full screen for the canvas; a swipe from an edge shows the bars briefly. */
    private fun hideSystemBars() {
        WindowCompat.setDecorFitsSystemWindows(window, false)
        WindowInsetsControllerCompat(window, window.decorView).apply {
            hide(WindowInsetsCompat.Type.systemBars())
            systemBarsBehavior = WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        }
    }

    // ---- Called from Rust (any thread) --------------------------------------------------------

    /** File › Open: the system picker. All files, since VectorCraft documents have no MIME type. */
    fun pickOpen() {
        runOnUiThread {
            try {
                openDocument.launch(arrayOf("*/*"))
            } catch (e: Exception) {
                Log.e(TAG, "picker failed", e)
                toast("Couldn't open the file picker: ${e.message}")
            }
        }
    }

    /** File › Place…: the system picker for one or more files to place in the document. */
    fun pickPlace() {
        runOnUiThread {
            try {
                placeDocuments.launch(arrayOf("*/*"))
            } catch (e: Exception) {
                Log.e(TAG, "picker failed", e)
                toast("Couldn't open the file picker: ${e.message}")
            }
        }
    }

    /**
     * Save or Export: write [bytes] as `Downloads/VectorCraft/[name]` through MediaStore. The same
     * name saved again in this session overwrites the file. Returns null on success, else a message.
     */
    fun saveToDownloads(name: String, bytes: ByteArray): String? {
        return try {
            val resolver = contentResolver
            val existing = savedUris[name]
            val uri = existing ?: run {
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, name)
                    put(MediaStore.Downloads.MIME_TYPE, mimeFor(name))
                    put(MediaStore.Downloads.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/" + FOLDER)
                    put(MediaStore.Downloads.IS_PENDING, 1)
                }
                resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: return "couldn't create $name in Downloads"
            }
            val out = resolver.openOutputStream(uri, "wt") ?: return "couldn't open $name for writing"
            out.use { it.write(bytes) }
            if (existing == null) {
                resolver.update(uri, ContentValues().apply { put(MediaStore.Downloads.IS_PENDING, 0) }, null, null)
                savedUris[name] = uri
            }
            toast("Saved to Downloads/$FOLDER/$name")
            null
        } catch (e: Exception) {
            Log.e(TAG, "save failed", e)
            e.message ?: e.toString()
        }
    }

    /** Help menu links, the Discord button, About and Home links. */
    fun openUrl(url: String) {
        runOnUiThread {
            try {
                startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)))
            } catch (e: Exception) {
                Log.e(TAG, "open url failed", e)
                toast("Couldn't open $url")
            }
        }
    }

    /**
     * Edit › Paste from the menu: the clipboard's text (SVG markup among it), or null when it holds
     * none. Read on the UI thread, which owns the clipboard, with a short wait from the caller.
     */
    fun readClipboard(): String? {
        var text: String? = null
        val done = CountDownLatch(1)
        runOnUiThread {
            try {
                val clipboard = getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                val clip = clipboard.primaryClip
                if (clip != null && clip.itemCount > 0) {
                    text = clip.getItemAt(0).coerceToText(this)?.toString()
                }
            } catch (e: Exception) {
                Log.e(TAG, "clipboard read failed", e)
            } finally {
                done.countDown()
            }
        }
        done.await(1, TimeUnit.SECONDS)
        return text
    }

    /** The user's languages as BCP 47 tags, most preferred first, comma-separated (the UI language). */
    fun localeTags(): String {
        val locales = resources.configuration.locales
        val tags = ArrayList<String>()
        for (i in 0 until locales.size()) tags.add(locales.get(i).toLanguageTag())
        return tags.joinToString(",")
    }

    /**
     * File › Print: hand the print-ready [pdf] to Android's print framework, whose dialog picks the
     * printer (or Save as PDF). Returns null on success, else a message.
     */
    fun printPdf(title: String, pdf: ByteArray): String? {
        return try {
            val file = File(cacheDir, "print.pdf")
            file.writeBytes(pdf)
            runOnUiThread {
                try {
                    val manager = getSystemService(Context.PRINT_SERVICE) as PrintManager
                    manager.print(title, PdfPrintAdapter(file, title), PrintAttributes.Builder().build())
                } catch (e: Exception) {
                    Log.e(TAG, "print failed", e)
                    toast("Couldn't print: ${e.message}")
                }
            }
            null
        } catch (e: Exception) {
            Log.e(TAG, "print failed", e)
            e.message ?: e.toString()
        }
    }

    /** A print job that is a finished PDF: the framework copies it as it is. */
    private class PdfPrintAdapter(private val file: File, private val title: String) : PrintDocumentAdapter() {
        override fun onLayout(
            oldAttributes: PrintAttributes?,
            newAttributes: PrintAttributes,
            cancellationSignal: CancellationSignal?,
            callback: LayoutResultCallback,
            extras: Bundle?,
        ) {
            if (cancellationSignal?.isCanceled == true) {
                callback.onLayoutCancelled()
                return
            }
            val info = PrintDocumentInfo.Builder("$title.pdf")
                .setContentType(PrintDocumentInfo.CONTENT_TYPE_DOCUMENT)
                .build()
            callback.onLayoutFinished(info, true)
        }

        override fun onWrite(
            pages: Array<out PageRange>?,
            destination: ParcelFileDescriptor,
            cancellationSignal: CancellationSignal?,
            callback: WriteResultCallback,
        ) {
            try {
                FileInputStream(file).use { input ->
                    FileOutputStream(destination.fileDescriptor).use { output -> input.copyTo(output) }
                }
                callback.onWriteFinished(arrayOf(PageRange.ALL_PAGES))
            } catch (e: Exception) {
                Log.e(TAG, "print write failed", e)
                callback.onWriteFailed(e.message)
            }
        }
    }

    // ---- Helpers ------------------------------------------------------------------------------

    private fun displayName(uri: Uri): String? {
        contentResolver.query(uri, arrayOf(OpenableColumns.DISPLAY_NAME), null, null, null)?.use { c ->
            if (c.moveToFirst()) {
                val i = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
                if (i >= 0) return c.getString(i)
            }
        }
        return uri.lastPathSegment
    }

    private fun mimeFor(name: String): String = when (name.substringAfterLast('.', "").lowercase()) {
        "svg", "svgz" -> "image/svg+xml"
        "pdf" -> "application/pdf"
        "ai", "ait", "eps" -> "application/postscript"
        "png" -> "image/png"
        "jpg", "jpeg" -> "image/jpeg"
        "gif" -> "image/gif"
        "webp" -> "image/webp"
        "tif", "tiff" -> "image/tiff"
        "bmp" -> "image/bmp"
        "dxf" -> "image/vnd.dxf"
        "emf" -> "image/emf"
        "wmf" -> "image/wmf"
        "txt" -> "text/plain"
        "json" -> "application/json"
        "html", "htm" -> "text/html"
        "css" -> "text/css"
        "zip" -> "application/zip"
        else -> "application/octet-stream"
    }

    private fun toast(text: String) {
        runOnUiThread { Toast.makeText(this, text, Toast.LENGTH_SHORT).show() }
    }
}
