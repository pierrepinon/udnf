/*
 * gnome-software plugin for udnf: offers the apps of the system AppStream catalog as
 * user-scope alternatives ("udnf (utilisateur)"), installed without root by the udnf CLI.
 *
 * The plugin never links libdnf5: every operation runs `udnf --json ...` in a worker thread
 * and reads one JSON value per line. Package availability, installed packages and packages
 * refused by udnf are cached in memory (refusals also on disk, per version, so a refused app
 * is not offered again in the next session); only refresh_metadata touches the network.
 */

#include <glib/gstdio.h>
#include <gnome-software.h>
#include <gs-appstream.h>
#include <json-glib/json-glib.h>
#include <xmlb.h>

#include "gs-plugin-udnf.h"

#define UDNF_ORIGIN "udnf"
#define UDNF_ORIGIN_UI "udnf (utilisateur)"
#define COMPONENTS_APP_ID "udnf-components"

struct _GsPluginUdnf {
	GsPlugin	 parent;

	/* Serializes udnf runs (udnf refuses concurrent transactions). Only worker threads take it. */
	GMutex		 exec_mutex;

	/* Set once by setup, read-only afterwards. */
	gchar		*udnf;		/* path of the udnf binary */
	XbSilo		*silo;		/* system AppStream catalog */
	GHashTable	*components;	/* app id -> XbNode (component) */
	GHashTable	*ids_by_pkg;	/* pkgname -> GPtrArray of app ids */

	/* Caches: protected by @mutex, which is never held while udnf runs (refine takes it on
	 * gnome-software's main thread). */
	GMutex		 mutex;
	GHashTable	*available;	/* pkgname -> JsonObject (udnf available) */
	GHashTable	*installed;	/* pkgname -> JsonObject (udnf status), NULL when stale */
	GHashTable	*refused;	/* pkgname -> evr refused by udnf (scriptlets) */
	GHashTable	*apps;		/* app id -> GsApp created by this plugin */
};

G_DEFINE_TYPE (GsPluginUdnf, gs_plugin_udnf, GS_TYPE_PLUGIN)

/* ------------------------------------------------------------------------------------------ */
/* Running udnf                                                                                */
/* ------------------------------------------------------------------------------------------ */

typedef void (*UdnfLineFunc) (JsonObject *line, gpointer user_data);

/* Runs `udnf --json <args>` and calls @func for each JSON object printed. On failure, the
 * error carries udnf's message; @kind_out receives the error kind ("scriptlets", "key"...). */
static gboolean
run_udnf (GsPluginUdnf  *self,
          const gchar  **args,
          UdnfLineFunc   func,
          gpointer       user_data,
          gchar        **kind_out,
          GCancellable  *cancellable,
          GError       **error)
{
	g_autoptr(GPtrArray) argv = g_ptr_array_new ();
	g_autoptr(GSubprocess) proc = NULL;
	g_autoptr(GDataInputStream) out = NULL;
	g_autofree gchar *error_message = NULL;
	g_autofree gchar *error_kind = NULL;
	g_autoptr(GError) local_error = NULL;
	g_autoptr(GMutexLocker) exec_locker = g_mutex_locker_new (&self->exec_mutex);

	g_ptr_array_add (argv, self->udnf);
	g_ptr_array_add (argv, (gpointer) "--json");
	for (guint i = 0; args[i] != NULL; i++)
		g_ptr_array_add (argv, (gpointer) args[i]);
	g_ptr_array_add (argv, NULL);

	proc = g_subprocess_newv ((const gchar * const *) argv->pdata,
				  G_SUBPROCESS_FLAGS_STDOUT_PIPE | G_SUBPROCESS_FLAGS_STDERR_SILENCE,
				  error);
	if (proc == NULL)
		return FALSE;

	out = g_data_input_stream_new (g_subprocess_get_stdout_pipe (proc));
	g_data_input_stream_set_newline_type (out, G_DATA_STREAM_NEWLINE_TYPE_LF);
	while (TRUE) {
		g_autofree gchar *line = g_data_input_stream_read_line_utf8 (out, NULL, cancellable, &local_error);
		g_autoptr(JsonNode) node = NULL;
		JsonObject *obj;
		const gchar *event;

		if (line == NULL)
			break;
		node = json_from_string (line, NULL);
		if (node == NULL || !JSON_NODE_HOLDS_OBJECT (node))
			continue;
		obj = json_node_get_object (node);
		event = json_object_get_string_member_with_default (obj, "event", NULL);
		if (g_strcmp0 (event, "error") == 0) {
			g_free (error_message);
			error_message = g_strdup (json_object_get_string_member_with_default (obj, "message", ""));
			g_free (error_kind);
			error_kind = g_strdup (json_object_get_string_member_with_default (obj, "kind", ""));
			continue;
		}
		if (func != NULL)
			func (obj, user_data);
	}
	if (local_error != NULL) {
		g_subprocess_force_exit (proc);
		g_propagate_error (error, g_steal_pointer (&local_error));
		return FALSE;
	}
	if (!g_subprocess_wait (proc, cancellable, error))
		return FALSE;
	if (!g_subprocess_get_successful (proc)) {
		if (kind_out != NULL)
			*kind_out = g_steal_pointer (&error_kind);
		g_set_error_literal (error, GS_PLUGIN_ERROR, GS_PLUGIN_ERROR_FAILED,
				     error_message != NULL ? error_message : "udnf a échoué");
		return FALSE;
	}
	return TRUE;
}

/* ------------------------------------------------------------------------------------------ */
/* Caches                                                                                      */
/* ------------------------------------------------------------------------------------------ */

static void
add_by_name_cb (JsonObject *obj, gpointer user_data)
{
	GHashTable *table = user_data;
	const gchar *name = json_object_get_string_member_with_default (obj, "name", NULL);
	if (name != NULL)
		g_hash_table_insert (table, g_strdup (name), json_object_ref (obj));
}

static GHashTable *
new_object_table (void)
{
	return g_hash_table_new_full (g_str_hash, g_str_equal, g_free, (GDestroyNotify) json_object_unref);
}

/* Loads the system AppStream catalog and indexes the components that name a package. */
static gboolean
load_catalog (GsPluginUdnf *self, GCancellable *cancellable, GError **error)
{
	const gchar *dirs[] = { "/usr/share/swcatalog/xml", "/var/cache/swcatalog/xml", NULL };
	g_autoptr(XbBuilder) builder = xb_builder_new ();
	g_autoptr(GFile) blob = NULL;
	g_autofree gchar *blobfn = NULL;
	g_autoptr(GPtrArray) components = NULL;
	g_autoptr(XbSilo) silo = NULL;

	gs_appstream_add_current_locales (builder);
	for (guint i = 0; dirs[i] != NULL; i++) {
		g_autoptr(GDir) dir = g_dir_open (dirs[i], 0, NULL);
		const gchar *fn;
		if (dir == NULL)
			continue;
		while ((fn = g_dir_read_name (dir)) != NULL) {
			g_autofree gchar *path = NULL;
			g_autoptr(GFile) file = NULL;
			g_autoptr(XbBuilderSource) source = NULL;
			if (!g_str_has_suffix (fn, ".xml") && !g_str_has_suffix (fn, ".xml.gz"))
				continue;
			path = g_build_filename (dirs[i], fn, NULL);
			file = g_file_new_for_path (path);
			source = xb_builder_source_new ();
			if (!xb_builder_source_load_file (source, file, XB_BUILDER_SOURCE_FLAG_NONE, cancellable, NULL))
				continue;
			xb_builder_import_source (builder, source);
		}
	}

	blobfn = gs_utils_get_cache_filename ("udnf", "catalog.xmlb",
					      GS_UTILS_CACHE_FLAG_WRITEABLE | GS_UTILS_CACHE_FLAG_CREATE_DIRECTORY,
					      error);
	if (blobfn == NULL)
		return FALSE;
	blob = g_file_new_for_path (blobfn);
	silo = xb_builder_ensure (builder, blob, XB_BUILDER_COMPILE_FLAG_IGNORE_INVALID | XB_BUILDER_COMPILE_FLAG_SINGLE_LANG,
				  cancellable, error);
	if (silo == NULL)
		return FALSE;

	components = xb_silo_query (silo, "components/component/pkgname/..", 0, NULL);
	g_hash_table_remove_all (self->components);
	g_hash_table_remove_all (self->ids_by_pkg);
	for (guint i = 0; components != NULL && i < components->len; i++) {
		XbNode *component = g_ptr_array_index (components, i);
		const gchar *id = xb_node_query_text (component, "id", NULL);
		const gchar *pkg = xb_node_query_text (component, "pkgname", NULL);
		GPtrArray *ids;
		if (id == NULL || pkg == NULL || g_hash_table_contains (self->components, id))
			continue;
		g_hash_table_insert (self->components, g_strdup (id), g_object_ref (component));
		ids = g_hash_table_lookup (self->ids_by_pkg, pkg);
		if (ids == NULL) {
			ids = g_ptr_array_new_with_free_func (g_free);
			g_hash_table_insert (self->ids_by_pkg, g_strdup (pkg), ids);
		}
		g_ptr_array_add (ids, g_strdup (id));
	}
	g_set_object (&self->silo, silo);
	g_debug ("catalog: %u components with a package", g_hash_table_size (self->components));
	return TRUE;
}

/* Fills the available table from the metadata cache; @online allows a download when no
 * cache exists yet. */
static gboolean
load_available (GsPluginUdnf *self, gboolean online, GCancellable *cancellable, GError **error)
{
	g_autoptr(GPtrArray) args = g_ptr_array_new ();
	g_autoptr(GHashTable) table = new_object_table ();
	g_autoptr(GError) local_error = NULL;
	g_autoptr(GMutexLocker) locker = NULL;
	GHashTableIter iter;
	gpointer key;

	if (!online)
		g_ptr_array_add (args, (gpointer) "-C");
	g_ptr_array_add (args, (gpointer) "available");
	g_hash_table_iter_init (&iter, self->ids_by_pkg);
	while (g_hash_table_iter_next (&iter, &key, NULL))
		g_ptr_array_add (args, key);
	g_ptr_array_add (args, NULL);

	if (!run_udnf (self, (const gchar **) args->pdata, add_by_name_cb, table, NULL, cancellable, &local_error)) {
		if (!online) {
			/* No metadata cache yet: fetch it once. */
			g_debug ("no udnf metadata cache (%s), downloading", local_error->message);
			return load_available (self, TRUE, cancellable, error);
		}
		g_propagate_error (error, g_steal_pointer (&local_error));
		return FALSE;
	}
	locker = g_mutex_locker_new (&self->mutex);
	g_clear_pointer (&self->available, g_hash_table_unref);
	self->available = g_steal_pointer (&table);
	return TRUE;
}

/* Loads `udnf status` unless cached. Must be called without @mutex held. */
static gboolean
ensure_installed (GsPluginUdnf *self, GCancellable *cancellable, GError **error)
{
	const gchar *args[] = { "status", NULL };
	g_autoptr(GHashTable) table = NULL;

	{
		g_autoptr(GMutexLocker) locker = g_mutex_locker_new (&self->mutex);
		if (self->installed != NULL)
			return TRUE;
	}
	table = new_object_table ();
	if (!run_udnf (self, args, add_by_name_cb, table, NULL, cancellable, error))
		return FALSE;
	{
		g_autoptr(GMutexLocker) locker = g_mutex_locker_new (&self->mutex);
		if (self->installed == NULL)
			self->installed = g_steal_pointer (&table);
	}
	return TRUE;
}

static void
invalidate_installed (GsPluginUdnf *self)
{
	g_autoptr(GMutexLocker) locker = g_mutex_locker_new (&self->mutex);
	g_clear_pointer (&self->installed, g_hash_table_unref);
}

static gboolean
is_requested (GsPluginUdnf *self, const gchar *pkg)
{
	JsonObject *obj = self->installed != NULL ? g_hash_table_lookup (self->installed, pkg) : NULL;
	return obj != NULL && json_object_get_boolean_member_with_default (obj, "requested", FALSE);
}

/* ------------------------------------------------------------------------------------------ */
/* Apps                                                                                        */
/* ------------------------------------------------------------------------------------------ */

static const gchar *
app_pkgname (GsApp *app)
{
	return gs_app_get_metadata_item (app, "udnf::pkgname");
}

/* Version, sizes and state from the caches. */
static void
update_app_from_caches (GsPluginUdnf *self, GsApp *app)
{
	const gchar *pkg = app_pkgname (app);
	JsonObject *inst = (pkg != NULL && self->installed != NULL) ? g_hash_table_lookup (self->installed, pkg) : NULL;
	JsonObject *avail = (pkg != NULL && self->available != NULL) ? g_hash_table_lookup (self->available, pkg) : NULL;
	GsAppState state = gs_app_get_state (app);

	if (avail != NULL) {
		gs_app_set_size_download (app, GS_SIZE_TYPE_VALID,
					  json_object_get_int_member_with_default (avail, "download_size", 0));
		gs_app_set_size_installed (app, GS_SIZE_TYPE_VALID,
					   json_object_get_int_member_with_default (avail, "install_size", 0));
	}
	if (inst != NULL) {
		gs_app_set_version (app, json_object_get_string_member_with_default (inst, "evr", NULL));
		if (state == GS_APP_STATE_UNKNOWN || state == GS_APP_STATE_AVAILABLE)
			gs_app_set_state (app, GS_APP_STATE_INSTALLED);
	} else if (avail != NULL) {
		gs_app_set_version (app, json_object_get_string_member_with_default (avail, "evr", NULL));
		if (state == GS_APP_STATE_UNKNOWN || state == GS_APP_STATE_INSTALLED)
			gs_app_set_state (app, GS_APP_STATE_AVAILABLE);
	}
}

/* The user-scope app for catalog component @id (created once, then reused). */
static GsApp *
get_app (GsPluginUdnf *self, const gchar *id, GError **error)
{
	GsApp *app = g_hash_table_lookup (self->apps, id);
	XbNode *component;
	g_autoptr(GsApp) new_app = NULL;

	if (app != NULL)
		return g_object_ref (app);
	component = g_hash_table_lookup (self->components, id);
	if (component == NULL)
		return NULL;

	new_app = gs_app_new (NULL);
	if (!gs_appstream_refine_app (GS_PLUGIN (self), new_app, self->silo, component,
				      (GsPluginRefineRequireFlags) ~0, NULL, "",
				      AS_COMPONENT_SCOPE_USER, error))
		return NULL;
	gs_app_set_scope (new_app, AS_COMPONENT_SCOPE_USER);
	gs_app_set_bundle_kind (new_app, AS_BUNDLE_KIND_PACKAGE);
	/* The origin stays the catalog's ("fedora"), which gnome-software won't let change; the
	 * user scope keeps the unique id apart from the system app. */
	gs_app_set_origin_ui (new_app, UDNF_ORIGIN_UI);
	gs_app_set_metadata (new_app, "udnf::pkgname", xb_node_query_text (component, "pkgname", NULL));
	gs_app_set_metadata (new_app, "GnomeSoftware::PackagingFormat", "udnf");
	gs_app_set_management_plugin (new_app, GS_PLUGIN (self));
	update_app_from_caches (self, new_app);
	g_hash_table_insert (self->apps, g_strdup (id), g_object_ref (new_app));
	return g_steal_pointer (&new_app);
}

static gchar *
refused_path (GError **error)
{
	return gs_utils_get_cache_filename ("udnf", "refused",
					    GS_UTILS_CACHE_FLAG_WRITEABLE | GS_UTILS_CACHE_FLAG_CREATE_DIRECTORY,
					    error);
}

/* Refusals cache: one "pkgname<TAB>evr" per line. */
static void
load_refused (GsPluginUdnf *self)
{
	g_autofree gchar *path = refused_path (NULL);
	g_autofree gchar *data = NULL;
	g_auto(GStrv) lines = NULL;

	if (path == NULL || !g_file_get_contents (path, &data, NULL, NULL))
		return;
	lines = g_strsplit (data, "\n", -1);
	for (guint i = 0; lines[i] != NULL; i++) {
		g_auto(GStrv) fields = g_strsplit (lines[i], "\t", 2);
		if (fields[0] != NULL && fields[1] != NULL)
			g_hash_table_insert (self->refused, g_strdup (fields[0]), g_strdup (fields[1]));
	}
}

static void
add_refused (GsPluginUdnf *self, const gchar *pkg)
{
	JsonObject *avail = self->available != NULL ? g_hash_table_lookup (self->available, pkg) : NULL;
	g_autofree gchar *path = refused_path (NULL);
	g_autoptr(GString) data = g_string_new (NULL);
	GHashTableIter iter;
	gpointer key, value;

	g_hash_table_insert (self->refused, g_strdup (pkg),
			     g_strdup (avail != NULL ? json_object_get_string_member_with_default (avail, "evr", "") : ""));
	g_hash_table_iter_init (&iter, self->refused);
	while (g_hash_table_iter_next (&iter, &key, &value))
		g_string_append_printf (data, "%s\t%s\n", (gchar *) key, (gchar *) value);
	if (path != NULL)
		g_file_set_contents (path, data->str, data->len, NULL);
}

/* Whether udnf can offer @pkg: installed by udnf, or installable and not refused in this
 * version. */
static gboolean
can_offer (GsPluginUdnf *self, const gchar *pkg)
{
	const gchar *refused_evr = g_hash_table_lookup (self->refused, pkg);
	JsonObject *avail = self->available != NULL ? g_hash_table_lookup (self->available, pkg) : NULL;

	if (refused_evr != NULL && avail != NULL &&
	    g_strcmp0 (refused_evr, json_object_get_string_member_with_default (avail, "evr", NULL)) == 0)
		return FALSE;
	if (self->installed != NULL && g_hash_table_contains (self->installed, pkg))
		return TRUE;
	return self->available != NULL && g_hash_table_contains (self->available, pkg);
}

/* Proxy app carrying the updates of packages that are not catalog apps (libraries...). */
static GsApp *
get_components_app (GsPluginUdnf *self, guint n_updates)
{
	GsApp *app = g_hash_table_lookup (self->apps, COMPONENTS_APP_ID);
	g_autofree gchar *summary = NULL;

	if (app == NULL) {
		app = gs_app_new (COMPONENTS_APP_ID);
		gs_app_set_kind (app, AS_COMPONENT_KIND_GENERIC);
		gs_app_set_scope (app, AS_COMPONENT_SCOPE_USER);
		gs_app_set_bundle_kind (app, AS_BUNDLE_KIND_PACKAGE);
		gs_app_set_origin (app, UDNF_ORIGIN);
		gs_app_set_origin_ui (app, UDNF_ORIGIN_UI);
		gs_app_set_name (app, GS_APP_QUALITY_NORMAL, "Composants udnf");
		gs_app_add_quirk (app, GS_APP_QUIRK_COMPULSORY);
		gs_app_set_metadata (app, "udnf::upgrade-all", "1");
		gs_app_set_management_plugin (app, GS_PLUGIN (self));
		g_hash_table_insert (self->apps, g_strdup (COMPONENTS_APP_ID), app);
	}
	summary = g_strdup_printf ("%u bibliothèque(s) et dépendance(s) installées par udnf", n_updates);
	gs_app_set_summary (app, GS_APP_QUALITY_NORMAL, summary);
	gs_app_set_state (app, GS_APP_STATE_UPDATABLE_LIVE);
	return g_object_ref (app);
}

/* ------------------------------------------------------------------------------------------ */
/* setup                                                                                       */
/* ------------------------------------------------------------------------------------------ */

static void
setup_thread_cb (GTask *task, gpointer source_object, gpointer task_data, GCancellable *cancellable)
{
	GsPluginUdnf *self = GS_PLUGIN_UDNF (source_object);
	g_autoptr(GError) error = NULL;
	const gchar *env = g_getenv ("UDNF_BINARY");

	self->udnf = env != NULL ? g_strdup (env) : g_find_program_in_path ("udnf");
	if (self->udnf == NULL && g_file_test ("/usr/bin/udnf", G_FILE_TEST_IS_EXECUTABLE))
		self->udnf = g_strdup ("/usr/bin/udnf");
	if (self->udnf == NULL) {
		g_task_return_new_error (task, GS_PLUGIN_ERROR, GS_PLUGIN_ERROR_NOT_SUPPORTED, "udnf introuvable");
		return;
	}
	{
		g_autoptr(GMutexLocker) locker = g_mutex_locker_new (&self->mutex);
		load_refused (self);
	}
	if (!load_catalog (self, cancellable, &error) ||
	    !load_available (self, FALSE, cancellable, &error) ||
	    !ensure_installed (self, cancellable, &error)) {
		g_task_return_error (task, g_steal_pointer (&error));
		return;
	}
	g_task_return_boolean (task, TRUE);
}

static void
gs_plugin_udnf_setup_async (GsPlugin *plugin, GCancellable *cancellable,
                            GAsyncReadyCallback callback, gpointer user_data)
{
	g_autoptr(GTask) task = g_task_new (plugin, cancellable, callback, user_data);
	g_task_set_source_tag (task, gs_plugin_udnf_setup_async);
	g_task_run_in_thread (task, setup_thread_cb);
}

static gboolean
gs_plugin_udnf_setup_finish (GsPlugin *plugin, GAsyncResult *result, GError **error)
{
	return g_task_propagate_boolean (G_TASK (result), error);
}

/* ------------------------------------------------------------------------------------------ */
/* list_apps                                                                                   */
/* ------------------------------------------------------------------------------------------ */

static void
add_apps_for_pkg (GsPluginUdnf *self, GsAppList *list, const gchar *pkg)
{
	GPtrArray *ids = g_hash_table_lookup (self->ids_by_pkg, pkg);
	for (guint i = 0; ids != NULL && i < ids->len; i++) {
		g_autoptr(GsApp) app = get_app (self, g_ptr_array_index (ids, i), NULL);
		if (app != NULL)
			gs_app_list_add (list, app);
	}
}

static void
collect_plan_cb (JsonObject *obj, gpointer user_data)
{
	GPtrArray *plans = user_data;
	if (g_strcmp0 (json_object_get_string_member_with_default (obj, "event", NULL), "plan") == 0)
		g_ptr_array_add (plans, json_object_ref (obj));
}

/* Apps for the updates of `udnf upgrade --dry-run` plans. Called with @mutex held. */
static void
add_updates (GsPluginUdnf *self, GsAppList *list, GPtrArray *plans)
{
	guint others = 0;

	for (guint p = 0; p < plans->len; p++) {
		JsonArray *updates = json_object_get_array_member (g_ptr_array_index (plans, p), "update");
		for (guint i = 0; updates != NULL && i < json_array_get_length (updates); i++) {
			JsonObject *u = json_array_get_object_element (updates, i);
			const gchar *name = json_object_get_string_member_with_default (u, "name", NULL);
			GPtrArray *ids = name != NULL ? g_hash_table_lookup (self->ids_by_pkg, name) : NULL;

			if (ids == NULL || !is_requested (self, name)) {
				others++;
				continue;
			}
			for (guint j = 0; j < ids->len; j++) {
				g_autoptr(GsApp) app = get_app (self, g_ptr_array_index (ids, j), NULL);
				if (app == NULL)
					continue;
				gs_app_set_update_version (app, json_object_get_string_member_with_default (u, "to", NULL));
				gs_app_set_state (app, GS_APP_STATE_UPDATABLE_LIVE);
				gs_app_list_add (list, app);
			}
		}
	}
	if (others > 0) {
		g_autoptr(GsApp) components = get_components_app (self, others);
		gs_app_list_add (list, components);
	}
}

static void
list_apps_thread_cb (GTask *task, gpointer source_object, gpointer task_data, GCancellable *cancellable)
{
	GsPluginUdnf *self = GS_PLUGIN_UDNF (source_object);
	GsAppQuery *query = task_data;
	g_autoptr(GMutexLocker) locker = NULL;
	g_autoptr(GsAppList) list = gs_app_list_new ();
	g_autoptr(GsAppList) found = gs_app_list_new ();
	g_autoptr(GPtrArray) plans = g_ptr_array_new_with_free_func ((GDestroyNotify) json_object_unref);
	g_autoptr(GError) error = NULL;
	GsApp *alternate_of = gs_app_query_get_alternate_of (query);

	/* Everything that runs udnf or searches the (immutable) catalog, without the lock. */
	if (gs_app_query_get_keywords (query) != NULL &&
	    !gs_appstream_search (GS_PLUGIN (self), self->silo, gs_app_query_get_keywords (query),
				  found, cancellable, &error)) {
		g_task_return_error (task, g_steal_pointer (&error));
		return;
	}
	if (gs_app_query_get_is_for_update (query) == GS_APP_QUERY_TRISTATE_TRUE) {
		const gchar *args[] = { "-C", "upgrade", "--dry-run", NULL };
		if (!run_udnf (self, args, collect_plan_cb, plans, NULL, cancellable, &error)) {
			g_task_return_error (task, g_steal_pointer (&error));
			return;
		}
	}

	/* A transaction may invalidate the installed cache between the load and the lock: load again
	 * (a few times at most) instead of answering from an empty cache. */
	for (guint attempt = 0; attempt < 3; attempt++) {
		if (!ensure_installed (self, cancellable, &error)) {
			g_task_return_error (task, g_steal_pointer (&error));
			return;
		}
		locker = g_mutex_locker_new (&self->mutex);
		if (self->installed != NULL)
			break;
		g_clear_pointer (&locker, g_mutex_locker_free);
	}
	if (locker == NULL) {
		g_task_return_new_error (task, GS_PLUGIN_ERROR, GS_PLUGIN_ERROR_FAILED,
					 "udnf: liste des paquets installés indisponible");
		return;
	}

	if (alternate_of != NULL) {
		const gchar *id = gs_app_get_id (alternate_of);
		XbNode *component = id != NULL ? g_hash_table_lookup (self->components, id) : NULL;
		const gchar *pkg = component != NULL ? xb_node_query_text (component, "pkgname", NULL) : NULL;
		if (pkg != NULL && can_offer (self, pkg)) {
			g_autoptr(GsApp) app = get_app (self, id, NULL);
			if (app != NULL)
				gs_app_list_add (list, app);
		}
	}

	/* Search: gnome-software's own ranking on the catalog, then the user-scope apps udnf can
	 * offer. */
	{
		for (guint i = 0; i < gs_app_list_length (found); i++) {
			GsApp *hit = gs_app_list_index (found, i);
			const gchar *id = gs_app_get_id (hit);
			XbNode *component = id != NULL ? g_hash_table_lookup (self->components, id) : NULL;
			const gchar *pkg = component != NULL ? xb_node_query_text (component, "pkgname", NULL) : NULL;
			g_autoptr(GsApp) app = NULL;
			if (pkg == NULL || !can_offer (self, pkg))
				continue;
			app = get_app (self, id, NULL);
			if (app == NULL)
				continue;
			gs_app_set_match_value (app, gs_app_get_match_value (hit));
			gs_app_list_add (list, app);
		}
	}

	if (gs_app_query_get_is_installed (query) == GS_APP_QUERY_TRISTATE_TRUE) {
		GHashTableIter iter;
		gpointer key;
		g_hash_table_iter_init (&iter, self->installed);
		while (g_hash_table_iter_next (&iter, &key, NULL))
			if (is_requested (self, key))
				add_apps_for_pkg (self, list, key);
	}

	add_updates (self, list, plans);

	g_task_return_pointer (task, g_steal_pointer (&list), g_object_unref);
}

static void
gs_plugin_udnf_list_apps_async (GsPlugin *plugin, GsAppQuery *query, GsPluginListAppsFlags flags,
                                GsPluginEventCallback event_callback, void *event_user_data,
                                GCancellable *cancellable, GAsyncReadyCallback callback, gpointer user_data)
{
	g_autoptr(GTask) task = g_task_new (plugin, cancellable, callback, user_data);
	g_task_set_source_tag (task, gs_plugin_udnf_list_apps_async);
	if (query == NULL) {
		g_task_return_pointer (task, gs_app_list_new (), g_object_unref);
		return;
	}
	g_task_set_task_data (task, g_object_ref (query), g_object_unref);
	g_task_run_in_thread (task, list_apps_thread_cb);
}

static GsAppList *
gs_plugin_udnf_list_apps_finish (GsPlugin *plugin, GAsyncResult *result, GError **error)
{
	return g_task_propagate_pointer (G_TASK (result), error);
}

/* ------------------------------------------------------------------------------------------ */
/* refine                                                                                      */
/* ------------------------------------------------------------------------------------------ */

static void
gs_plugin_udnf_refine_async (GsPlugin *plugin, GsAppList *list, GsPluginRefineFlags job_flags,
                             GsPluginRefineRequireFlags require_flags,
                             GsPluginEventCallback event_callback, void *event_user_data,
                             GCancellable *cancellable, GAsyncReadyCallback callback, gpointer user_data)
{
	GsPluginUdnf *self = GS_PLUGIN_UDNF (plugin);
	g_autoptr(GTask) task = g_task_new (plugin, cancellable, callback, user_data);
	g_autoptr(GMutexLocker) locker = g_mutex_locker_new (&self->mutex);

	g_task_set_source_tag (task, gs_plugin_udnf_refine_async);
	/* Caches only: never blocks on udnf. */
	for (guint i = 0; i < gs_app_list_length (list); i++) {
		GsApp *app = gs_app_list_index (list, i);
		if (gs_app_has_management_plugin (app, plugin) && app_pkgname (app) != NULL)
			update_app_from_caches (self, app);
	}
	g_task_return_boolean (task, TRUE);
}

static gboolean
gs_plugin_udnf_refine_finish (GsPlugin *plugin, GAsyncResult *result, GError **error)
{
	return g_task_propagate_boolean (G_TASK (result), error);
}

/* ------------------------------------------------------------------------------------------ */
/* install / uninstall / update                                                                */
/* ------------------------------------------------------------------------------------------ */

typedef enum {
	OP_INSTALL,
	OP_REMOVE,
	OP_UPDATE,
} Op;

typedef struct {
	Op			 op;
	GsAppList		*apps;
	gboolean		 no_apply;
	gboolean		 interactive;
	GsPluginEventCallback	 event_callback;
	void			*event_user_data;
} OpData;

typedef struct {
	GsPlugin		*plugin;
	GsPluginEvent		*event;
	GsPluginEventCallback	 callback;
	void			*user_data;
} EventData;

static gboolean
event_idle_cb (gpointer user_data)
{
	EventData *data = user_data;
	data->callback (data->plugin, data->event, data->user_data);
	return G_SOURCE_REMOVE;
}

static void
event_data_free (EventData *data)
{
	g_object_unref (data->plugin);
	g_object_unref (data->event);
	g_free (data);
}

/* gnome-software jobs drop the errors plugins return: the user only sees what is reported
 * through the event callback, which must run in the job's main context. */
static void
report_error (GTask *task, OpData *op, GsApp *app, const GError *error)
{
	EventData *data;

	g_debug ("udnf: %s", error->message);
	if (op->event_callback == NULL || g_error_matches (error, G_IO_ERROR, G_IO_ERROR_CANCELLED))
		return;
	data = g_new0 (EventData, 1);
	data->plugin = g_object_ref (g_task_get_source_object (task));
	data->event = gs_plugin_event_new ("app", app, "error", error, NULL);
	gs_plugin_event_add_flag (data->event, GS_PLUGIN_EVENT_FLAG_WARNING);
	if (op->interactive)
		gs_plugin_event_add_flag (data->event, GS_PLUGIN_EVENT_FLAG_INTERACTIVE);
	data->callback = op->event_callback;
	data->user_data = op->event_user_data;
	g_main_context_invoke_full (g_task_get_context (task), G_PRIORITY_DEFAULT, event_idle_cb, data,
				    (GDestroyNotify) event_data_free);
}

static void
op_data_free (OpData *data)
{
	g_object_unref (data->apps);
	g_free (data);
}

static void
progress_cb (JsonObject *obj, gpointer user_data)
{
	GsApp *app = user_data;
	const gchar *event = json_object_get_string_member_with_default (obj, "event", NULL);

	if (g_strcmp0 (event, "download") == 0) {
		gs_app_set_progress (app, 10);
	} else if (g_strcmp0 (event, "unpack") == 0) {
		gint64 index = json_object_get_int_member_with_default (obj, "index", 0);
		gint64 total = json_object_get_int_member_with_default (obj, "total", 1);
		gs_app_set_progress (app, (guint) (20 + 80 * index / MAX (total, 1)));
	}
}

static gboolean
run_op (GsPluginUdnf *self, Op op, GsApp *app, GCancellable *cancellable, GError **error)
{
	const gchar *pkg = app_pkgname (app);
	gboolean all = gs_app_get_metadata_item (app, "udnf::upgrade-all") != NULL;
	const gchar *verb = op == OP_INSTALL ? "install" : op == OP_REMOVE ? "remove" : "upgrade";
	const gchar *args[] = { "-y", verb, all ? NULL : pkg, NULL };
	g_autofree gchar *kind = NULL;
	g_autoptr(GMutexLocker) locker = NULL;
	gboolean ok;

	if (pkg == NULL && !all)
		return TRUE;
	if (op == OP_REMOVE) {
		gboolean requested;
		if (!ensure_installed (self, cancellable, error))
			return FALSE;
		locker = g_mutex_locker_new (&self->mutex);
		requested = is_requested (self, pkg);
		g_clear_pointer (&locker, g_mutex_locker_free);
		if (!requested) {
			g_set_error (error, GS_PLUGIN_ERROR, GS_PLUGIN_ERROR_NOT_SUPPORTED,
				     "%s est une dépendance installée par udnf, pas une application demandée", pkg);
			return FALSE;
		}
	}

	gs_app_set_state (app, op == OP_REMOVE ? GS_APP_STATE_REMOVING : GS_APP_STATE_INSTALLING);
	gs_app_set_progress (app, GS_APP_PROGRESS_UNKNOWN);
	ok = run_udnf (self, args, progress_cb, app, &kind, cancellable, error);
	invalidate_installed (self);
	if (!ok) {
		if (g_strcmp0 (kind, "scriptlets") == 0 && pkg != NULL) {
			locker = g_mutex_locker_new (&self->mutex);
			add_refused (self, pkg);
			g_clear_pointer (&locker, g_mutex_locker_free);
		}
		gs_app_set_state_recover (app);
		return FALSE;
	}
	if (!ensure_installed (self, cancellable, error))
		return FALSE;
	if (all) {
		gs_app_set_state (app, GS_APP_STATE_INSTALLED);
		return TRUE;
	}
	gs_app_set_state (app, op == OP_REMOVE ? GS_APP_STATE_AVAILABLE : GS_APP_STATE_INSTALLED);
	locker = g_mutex_locker_new (&self->mutex);
	update_app_from_caches (self, app);
	return TRUE;
}

static void
op_thread_cb (GTask *task, gpointer source_object, gpointer task_data, GCancellable *cancellable)
{
	GsPluginUdnf *self = GS_PLUGIN_UDNF (source_object);
	OpData *data = task_data;

	/* udnf has no download-only mode: everything happens on apply. */
	if (data->no_apply) {
		g_task_return_boolean (task, TRUE);
		return;
	}
	/* One failing app (e.g. a refused package) must not prevent the others: report each error,
	 * return the first one. */
	g_autoptr(GError) first_error = NULL;
	for (guint i = 0; i < gs_app_list_length (data->apps); i++) {
		GsApp *app = gs_app_list_index (data->apps, i);
		g_autoptr(GError) error = NULL;
		if (!gs_app_has_management_plugin (app, GS_PLUGIN (self)))
			continue;
		if (g_cancellable_is_cancelled (cancellable))
			break;
		if (!run_op (self, data->op, app, cancellable, &error)) {
			report_error (task, data, app, error);
			if (first_error == NULL)
				first_error = g_steal_pointer (&error);
		}
	}
	if (first_error != NULL)
		g_task_return_error (task, g_steal_pointer (&first_error));
	else if (!g_task_return_error_if_cancelled (task))
		g_task_return_boolean (task, TRUE);
}

static void
start_op (GsPlugin *plugin, Op op, GsAppList *apps, gboolean no_apply, gboolean interactive,
          GsPluginEventCallback event_callback, void *event_user_data, GCancellable *cancellable,
          GAsyncReadyCallback callback, gpointer user_data, gpointer source_tag)
{
	g_autoptr(GTask) task = g_task_new (plugin, cancellable, callback, user_data);
	OpData *data = g_new0 (OpData, 1);

	data->op = op;
	data->apps = g_object_ref (apps);
	data->no_apply = no_apply;
	data->interactive = interactive;
	data->event_callback = event_callback;
	data->event_user_data = event_user_data;
	g_task_set_source_tag (task, source_tag);
	g_task_set_task_data (task, data, (GDestroyNotify) op_data_free);
	g_task_run_in_thread (task, op_thread_cb);
}

static gboolean
op_finish (GsPlugin *plugin, GAsyncResult *result, GError **error)
{
	return g_task_propagate_boolean (G_TASK (result), error);
}

static void
gs_plugin_udnf_install_apps_async (GsPlugin *plugin, GsAppList *apps, GsPluginInstallAppsFlags flags,
                                   GsPluginProgressCallback progress_callback, gpointer progress_user_data,
                                   GsPluginEventCallback event_callback, void *event_user_data,
                                   GsPluginAppNeedsUserActionCallback app_needs_user_action_callback,
                                   gpointer app_needs_user_action_data,
                                   GCancellable *cancellable, GAsyncReadyCallback callback, gpointer user_data)
{
	start_op (plugin, OP_INSTALL, apps, (flags & GS_PLUGIN_INSTALL_APPS_FLAGS_NO_APPLY) != 0,
		  (flags & GS_PLUGIN_INSTALL_APPS_FLAGS_INTERACTIVE) != 0, event_callback, event_user_data,
		  cancellable, callback, user_data, gs_plugin_udnf_install_apps_async);
}

static void
gs_plugin_udnf_uninstall_apps_async (GsPlugin *plugin, GsAppList *apps, GsPluginUninstallAppsFlags flags,
                                     GsPluginProgressCallback progress_callback, gpointer progress_user_data,
                                     GsPluginEventCallback event_callback, void *event_user_data,
                                     GsPluginAppNeedsUserActionCallback app_needs_user_action_callback,
                                     gpointer app_needs_user_action_data,
                                     GCancellable *cancellable, GAsyncReadyCallback callback, gpointer user_data)
{
	start_op (plugin, OP_REMOVE, apps, FALSE, (flags & GS_PLUGIN_UNINSTALL_APPS_FLAGS_INTERACTIVE) != 0,
		  event_callback, event_user_data, cancellable, callback, user_data,
		  gs_plugin_udnf_uninstall_apps_async);
}

static void
gs_plugin_udnf_update_apps_async (GsPlugin *plugin, GsAppList *apps, GsPluginUpdateAppsFlags flags,
                                  GsPluginProgressCallback progress_callback, gpointer progress_user_data,
                                  GsPluginEventCallback event_callback, void *event_user_data,
                                  GsPluginAppNeedsUserActionCallback app_needs_user_action_callback,
                                  gpointer app_needs_user_action_data,
                                  GCancellable *cancellable, GAsyncReadyCallback callback, gpointer user_data)
{
	start_op (plugin, OP_UPDATE, apps, (flags & GS_PLUGIN_UPDATE_APPS_FLAGS_NO_APPLY) != 0,
		  (flags & GS_PLUGIN_UPDATE_APPS_FLAGS_INTERACTIVE) != 0, event_callback, event_user_data,
		  cancellable, callback, user_data, gs_plugin_udnf_update_apps_async);
}

/* ------------------------------------------------------------------------------------------ */
/* refresh_metadata                                                                            */
/* ------------------------------------------------------------------------------------------ */

static void
refresh_thread_cb (GTask *task, gpointer source_object, gpointer task_data, GCancellable *cancellable)
{
	GsPluginUdnf *self = GS_PLUGIN_UDNF (source_object);
	guint64 cache_age_secs = *(guint64 *) task_data;
	g_autoptr(GError) error = NULL;
	g_autofree gchar *stamp = NULL;
	const gchar *args[] = { "makecache", NULL };
	GStatBuf st;

	stamp = gs_utils_get_cache_filename ("udnf", "refresh.stamp",
					     GS_UTILS_CACHE_FLAG_WRITEABLE | GS_UTILS_CACHE_FLAG_CREATE_DIRECTORY,
					     &error);
	if (stamp == NULL) {
		g_task_return_error (task, g_steal_pointer (&error));
		return;
	}
	if (g_stat (stamp, &st) == 0 && (guint64) (g_get_real_time () / G_USEC_PER_SEC - st.st_mtime) < cache_age_secs) {
		g_task_return_boolean (task, TRUE);
		return;
	}
	if (!run_udnf (self, args, NULL, NULL, NULL, cancellable, &error) ||
	    !load_available (self, FALSE, cancellable, &error) ||
	    !g_file_set_contents (stamp, "", 0, &error)) {
		g_task_return_error (task, g_steal_pointer (&error));
		return;
	}
	invalidate_installed (self);
	g_task_return_boolean (task, TRUE);
}

static void
gs_plugin_udnf_refresh_metadata_async (GsPlugin *plugin, guint64 cache_age_secs,
                                       GsPluginRefreshMetadataFlags flags,
                                       GsPluginEventCallback event_callback, void *event_user_data,
                                       GCancellable *cancellable, GAsyncReadyCallback callback,
                                       gpointer user_data)
{
	g_autoptr(GTask) task = g_task_new (plugin, cancellable, callback, user_data);
	guint64 *age = g_new (guint64, 1);

	*age = cache_age_secs;
	g_task_set_source_tag (task, gs_plugin_udnf_refresh_metadata_async);
	g_task_set_task_data (task, age, g_free);
	g_task_run_in_thread (task, refresh_thread_cb);
}

static gboolean
gs_plugin_udnf_refresh_metadata_finish (GsPlugin *plugin, GAsyncResult *result, GError **error)
{
	return g_task_propagate_boolean (G_TASK (result), error);
}

/* ------------------------------------------------------------------------------------------ */
/* launch, adopt                                                                               */
/* ------------------------------------------------------------------------------------------ */

static void
gs_plugin_udnf_launch_async (GsPlugin *plugin, GsApp *app, GsPluginLaunchFlags flags,
                             GCancellable *cancellable, GAsyncReadyCallback callback, gpointer user_data)
{
	/* udnf exports the desktop entry to ~/.local/share/applications under the same id. */
	gs_plugin_app_launch_async (plugin, app, flags, cancellable, callback, user_data);
}

static gboolean
gs_plugin_udnf_launch_finish (GsPlugin *plugin, GAsyncResult *result, GError **error)
{
	return gs_plugin_app_launch_finish (plugin, result, error);
}

static void
gs_plugin_udnf_adopt_app (GsPlugin *plugin, GsApp *app)
{
	if (gs_app_get_metadata_item (app, "udnf::pkgname") != NULL &&
	    gs_app_get_scope (app) == AS_COMPONENT_SCOPE_USER)
		gs_app_set_management_plugin (app, plugin);
}

/* ------------------------------------------------------------------------------------------ */
/* GObject                                                                                     */
/* ------------------------------------------------------------------------------------------ */

static void
gs_plugin_udnf_init (GsPluginUdnf *self)
{
	g_mutex_init (&self->mutex);
	g_mutex_init (&self->exec_mutex);
	self->components = g_hash_table_new_full (g_str_hash, g_str_equal, g_free, g_object_unref);
	self->ids_by_pkg = g_hash_table_new_full (g_str_hash, g_str_equal, g_free, (GDestroyNotify) g_ptr_array_unref);
	self->refused = g_hash_table_new_full (g_str_hash, g_str_equal, g_free, g_free);
	self->apps = g_hash_table_new_full (g_str_hash, g_str_equal, g_free, g_object_unref);
	gs_plugin_add_rule (GS_PLUGIN (self), GS_PLUGIN_RULE_RUN_AFTER, "appstream");
}

static void
gs_plugin_udnf_finalize (GObject *object)
{
	GsPluginUdnf *self = GS_PLUGIN_UDNF (object);

	g_free (self->udnf);
	g_clear_object (&self->silo);
	g_hash_table_unref (self->components);
	g_hash_table_unref (self->ids_by_pkg);
	g_clear_pointer (&self->available, g_hash_table_unref);
	g_clear_pointer (&self->installed, g_hash_table_unref);
	g_hash_table_unref (self->refused);
	g_hash_table_unref (self->apps);
	g_mutex_clear (&self->mutex);
	g_mutex_clear (&self->exec_mutex);
	G_OBJECT_CLASS (gs_plugin_udnf_parent_class)->finalize (object);
}

static void
gs_plugin_udnf_class_init (GsPluginUdnfClass *klass)
{
	GObjectClass *object_class = G_OBJECT_CLASS (klass);
	GsPluginClass *plugin_class = GS_PLUGIN_CLASS (klass);

	object_class->finalize = gs_plugin_udnf_finalize;

	plugin_class->adopt_app = gs_plugin_udnf_adopt_app;
	plugin_class->setup_async = gs_plugin_udnf_setup_async;
	plugin_class->setup_finish = gs_plugin_udnf_setup_finish;
	plugin_class->list_apps_async = gs_plugin_udnf_list_apps_async;
	plugin_class->list_apps_finish = gs_plugin_udnf_list_apps_finish;
	plugin_class->refine_async = gs_plugin_udnf_refine_async;
	plugin_class->refine_finish = gs_plugin_udnf_refine_finish;
	plugin_class->refresh_metadata_async = gs_plugin_udnf_refresh_metadata_async;
	plugin_class->refresh_metadata_finish = gs_plugin_udnf_refresh_metadata_finish;
	plugin_class->install_apps_async = gs_plugin_udnf_install_apps_async;
	plugin_class->install_apps_finish = op_finish;
	plugin_class->uninstall_apps_async = gs_plugin_udnf_uninstall_apps_async;
	plugin_class->uninstall_apps_finish = op_finish;
	plugin_class->update_apps_async = gs_plugin_udnf_update_apps_async;
	plugin_class->update_apps_finish = op_finish;
	plugin_class->launch_async = gs_plugin_udnf_launch_async;
	plugin_class->launch_finish = gs_plugin_udnf_launch_finish;
}

GType
gs_plugin_query_type (void)
{
	return GS_TYPE_PLUGIN_UDNF;
}
