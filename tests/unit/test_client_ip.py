"""Which address a request is counted against, and which requests are local.

Proxy headers are the client's to write unless a proxy wrote them. A request
that reaches the server straight from a client -- over the internet, or from
a LAN device when the server is exposed on the LAN -- carries whatever
``X-Forwarded-For`` it chose to send; trusted, a login flood rotates it past
the rate limit. Only a proxy is believed: this machine (the container's own
nginx) always, and any network named in ``server.trusted_proxies``. A proxy
sets ``X-Real-IP`` to the client; nginx also APPENDS the client to
``X-Forwarded-For``, so of that header only the rightmost entry is the
proxy's -- everything left of it came from the client.
"""

from __future__ import annotations

from typing import Any

import pytest

from mokuro_bunko import security
from mokuro_bunko.config import ServerConfig
from mokuro_bunko.security import get_client_ip
from mokuro_bunko.setup.api import SetupWizardAPI


def env(remote: str, **headers: str) -> dict[str, Any]:
    return {"REMOTE_ADDR": remote, **{f"HTTP_{k.upper()}": v for k, v in headers.items()}}


@pytest.fixture(autouse=True)
def _no_configured_proxies() -> Any:
    security.set_trusted_proxies([])
    yield
    security.set_trusted_proxies([])


class TestDirect:
    def test_a_public_peer_is_counted_as_itself_whatever_it_claims(self) -> None:
        assert get_client_ip(env("93.184.215.14", x_forwarded_for="1.2.3.4")) == "93.184.215.14"
        assert get_client_ip(env("93.184.215.14", x_real_ip="1.2.3.4")) == "93.184.215.14"

    def test_a_lan_client_is_not_a_proxy(self) -> None:
        # A server exposed straight on the LAN sees its clients' own private
        # addresses; they must not be able to pick their own rate-limit key.
        assert get_client_ip(env("192.168.1.50", x_forwarded_for="10.0.0.1")) == "192.168.1.50"
        assert get_client_ip(env("192.168.1.50", x_real_ip="10.0.0.1")) == "192.168.1.50"

    def test_no_headers_is_the_peer(self) -> None:
        assert get_client_ip(env("127.0.0.1")) == "127.0.0.1"


class TestBehindAProxy:
    def test_x_real_ip_from_a_local_proxy_is_the_client(self) -> None:
        assert get_client_ip(
            env("127.0.0.1", x_real_ip="198.51.100.7", x_forwarded_for="10.9.9.9, 198.51.100.7")
        ) == "198.51.100.7"

    def test_only_the_rightmost_forwarded_entry_is_the_proxys(self) -> None:
        # The client sent "1.2.3.4"; the proxy appended the real address.
        assert get_client_ip(
            env("127.0.0.1", x_forwarded_for="1.2.3.4, 198.51.100.7")
        ) == "198.51.100.7"

    def test_a_configured_proxy_network_is_believed(self) -> None:
        security.set_trusted_proxies(["172.16.0.0/12"])
        assert get_client_ip(env("172.18.0.3", x_real_ip="198.51.100.7")) == "198.51.100.7"
        assert get_client_ip(env("192.168.1.50", x_real_ip="10.0.0.1")) == "192.168.1.50"

    def test_rotating_the_client_half_of_the_header_changes_nothing(self) -> None:
        keys = {
            get_client_ip(env("127.0.0.1", x_forwarded_for=f"10.0.0.{n}, 198.51.100.7"))
            for n in range(20)
        }
        assert keys == {"198.51.100.7"}


class TestSetupIsLocalOnly:
    def test_a_spoofed_loopback_through_a_local_proxy_is_not_local(self) -> None:
        assert not SetupWizardAPI._is_local_request(
            env("127.0.0.1", x_forwarded_for="127.0.0.1, 198.51.100.7")
        )

    def test_a_proxied_remote_client_is_not_local(self) -> None:
        assert not SetupWizardAPI._is_local_request(env("127.0.0.1", x_real_ip="198.51.100.7"))

    def test_a_request_on_this_machine_is_local(self) -> None:
        assert SetupWizardAPI._is_local_request(env("127.0.0.1"))


class TestTheSetting:
    def test_loopback_only_by_default(self) -> None:
        assert ServerConfig().trusted_proxies == []

    def test_a_malformed_network_is_refused_at_load(self) -> None:
        with pytest.raises(ValueError, match="trusted_proxies"):
            ServerConfig(trusted_proxies=["not-a-network"])

    def test_the_environment_sets_it(self, monkeypatch: pytest.MonkeyPatch) -> None:
        from mokuro_bunko.config import Config, _apply_env_overrides

        monkeypatch.setenv("MOKURO_SERVER_TRUSTED_PROXIES", "172.16.0.0/12, 10.1.2.3")
        config = Config()
        _apply_env_overrides(config)
        assert config.server.trusted_proxies == ["172.16.0.0/12", "10.1.2.3"]

    def test_the_app_installs_it(self, tmp_path: Any) -> None:
        from mokuro_bunko.config import Config, StorageConfig
        from mokuro_bunko.server import create_app

        create_app(Config(
            server=ServerConfig(trusted_proxies=["172.16.0.0/12"]),
            storage=StorageConfig(base_path=tmp_path),
        ))
        assert get_client_ip(env("172.18.0.3", x_real_ip="198.51.100.7")) == "198.51.100.7"
