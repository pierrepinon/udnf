project('udnf-gnome-software', 'c', version : '0.1.0', meson_version : '>= 1.0')

gnome_software = dependency('gnome-software')
plugin_dir = gnome_software.get_variable(pkgconfig : 'plugindir')

shared_module(
  'gs_plugin_udnf',
  sources : 'gs-plugin-udnf.c',
  c_args : [
    '-DG_LOG_DOMAIN="GsPluginUdnf"',
    '-DI_KNOW_THE_GNOME_SOFTWARE_API_IS_SUBJECT_TO_CHANGE',
  ],
  dependencies : [
    gnome_software,
    dependency('xmlb'),
    dependency('json-glib-1.0'),
    dependency('gio-unix-2.0'),
    dependency('appstream'),
    dependency('gtk4'),
  ],
  install : true,
  install_dir : plugin_dir,
)
