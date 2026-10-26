"""Isolate PROJJSON oracle runs from installed and cached datum grids."""
import os
import shutil
import tempfile
from contextlib import contextmanager
from pathlib import Path

from osgeo import osr


@contextmanager
def no_grid_proj():
    paths = osr.GetPROJSearchPaths()
    network = osr.GetPROJEnableNetwork()
    database = next((Path(p) / 'proj.db' for p in paths if (Path(p) / 'proj.db').is_file()), None)
    if database is None:
        raise RuntimeError('Cannot find proj.db in the configured PROJ search paths')
    cache = Path(os.environ.get('XDG_CACHE_HOME', Path.home() / '.cache')) / 'terrio'
    cache.mkdir(parents=True, exist_ok=True)
    keys = ['PROJ_DATA', 'PROJ_LIB', 'PROJ_USER_WRITABLE_DIRECTORY', 'TERRIO_GRID_DIR',
            'TERRIO_GRID_CACHE', 'XDG_DATA_HOME', 'LOCALAPPDATA', 'TERRIO_NETWORK', 'PROJ_NETWORK']
    previous = {key: os.environ.get(key) for key in keys}
    with tempfile.TemporaryDirectory(prefix='projjson-no-grids-', dir=cache) as directory:
        shutil.copyfile(database, Path(directory) / 'proj.db')
        try:
            for key in keys:
                os.environ[key] = 'OFF' if key in ('TERRIO_NETWORK', 'PROJ_NETWORK') else directory
            osr.SetPROJSearchPaths([directory])
            osr.SetPROJEnableNetwork(False)
            yield
        finally:
            osr.SetPROJSearchPaths(paths)
            osr.SetPROJEnableNetwork(network)
            for key, value in previous.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
