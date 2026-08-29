from acme_api import handle
from acme_worker import work


def test_work():
    assert work((4, 5)) == handle(4, 5)
