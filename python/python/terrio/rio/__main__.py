"""The ``rio`` command line of terrio.rio: ``python -m terrio.rio info dem.tif``."""

import sys

from terrio.rio.cli import main

if __name__ == "__main__":
    sys.exit(main())
