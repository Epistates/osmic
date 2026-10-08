// Static viewer script. All archive-derived values arrive as validated JSON
// in the #map element's data-config attribute; nothing is interpolated here.
(function () {
  'use strict';
  var el = document.getElementById('map');
  var cfg = JSON.parse(el.dataset.config);
  var map = new maplibregl.Map({
    container: 'map',
    style: 'style.json',
    center: cfg.center,
    zoom: cfg.zoom,
    minZoom: cfg.minZoom,
    maxZoom: cfg.maxZoom
  });
  map.addControl(new maplibregl.NavigationControl());
  map.addControl(new maplibregl.ScaleControl());
  if (cfg.bounds) {
    map.once('load', function () {
      map.fitBounds(
        [[cfg.bounds[0], cfg.bounds[1]], [cfg.bounds[2], cfg.bounds[3]]],
        { padding: 40, maxZoom: cfg.maxZoom }
      );
    });
  }
  var coord = document.getElementById('coord');
  map.on('move', function () {
    var c = map.getCenter();
    coord.textContent = 'zoom ' + map.getZoom().toFixed(1) +
      ' · center ' + c.lng.toFixed(4) + ',' + c.lat.toFixed(4);
  });
})();
