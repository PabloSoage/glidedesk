package io.github.pablosoage.glidedesk.shell

import android.content.ContentProvider
import android.content.ContentValues
import android.database.Cursor
import android.net.Uri
import android.os.Binder
import android.os.Bundle
import android.os.Process

/**
 * Where the ADB helper ([ShellMain]) hands over its binder. Exported, so only callers that
 * are the shell user or root are listened to.
 */
class ShellBinderProvider : ContentProvider() {
    override fun onCreate() = true

    override fun call(method: String, arg: String?, extras: Bundle?): Bundle? {
        val uid = Binder.getCallingUid()
        if (uid != Process.SHELL_UID && uid != Process.ROOT_UID) {
            throw SecurityException("only the shell user may attach a helper")
        }
        if (method != METHOD_ATTACH) return null
        val binder = extras?.getBinder(KEY_BINDER) ?: return null
        ShellConnection.attach(binder, "ADB")
        return Bundle().apply { putBinder(KEY_BINDER, ShellConnection.appToken) }
    }

    override fun query(uri: Uri, projection: Array<String>?, selection: String?, selectionArgs: Array<String>?, sortOrder: String?): Cursor? = null
    override fun getType(uri: Uri): String? = null
    override fun insert(uri: Uri, values: ContentValues?): Uri? = null
    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<String>?) = 0
    override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<String>?) = 0

    companion object {
        /** Appended to the application id (the manifest uses the same). */
        const val AUTHORITY_SUFFIX = ".shell"
        const val METHOD_ATTACH = "attach"
        const val KEY_BINDER = "binder"
    }
}
