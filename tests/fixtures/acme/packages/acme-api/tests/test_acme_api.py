from acme_api import handle
from acme_core import add


def test_handle():
    assert handle(1, 2) == {"sum": add(1, 2)}
