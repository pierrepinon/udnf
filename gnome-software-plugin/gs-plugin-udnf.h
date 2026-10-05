#pragma once

#include <glib.h>
#include <glib-object.h>

G_BEGIN_DECLS

#define GS_TYPE_PLUGIN_UDNF (gs_plugin_udnf_get_type ())

G_DECLARE_FINAL_TYPE (GsPluginUdnf, gs_plugin_udnf, GS, PLUGIN_UDNF, GsPlugin)

G_END_DECLS
