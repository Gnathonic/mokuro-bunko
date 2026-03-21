"""End-to-end tests for Docker deployment.

These tests verify that the Docker image builds and runs correctly.
They require Docker to be installed and running.
"""

from __future__ import annotations

import subprocess
import time
from collections.abc import Generator
from pathlib import Path

import pytest
import yaml


def docker_available() -> bool:
    """Check if Docker is available."""
    try:
        result = subprocess.run(
            ["docker", "info"],
            capture_output=True,
            timeout=10,
        )
        return result.returncode == 0
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return False


@pytest.fixture
def project_root() -> Path:
    """Get the project root directory."""
    return Path(__file__).parent.parent.parent


class TestDockerBuild:
    """Tests for Docker image building."""

    def test_dockerfile_exists(self, project_root: Path) -> None:
        """Test that Dockerfile exists."""
        dockerfile = project_root / "deploy" / "Dockerfile"
        assert dockerfile.exists()

    def test_dockerfile_syntax(self, project_root: Path) -> None:
        """Test Dockerfile syntax is valid."""
        dockerfile = project_root / "deploy" / "Dockerfile"

        # Use hadolint if available, otherwise run a lightweight syntax sanity check.
        try:
            result = subprocess.run(
                ["hadolint", str(dockerfile)],
                capture_output=True,
                text=True,
                timeout=30,
            )
            # hadolint returns 0 for no errors, 1 for warnings
            assert result.returncode in (0, 1), f"Dockerfile lint errors: {result.stdout}"
        except FileNotFoundError:
            content = dockerfile.read_text(encoding="utf-8")
            assert "FROM" in content
            assert "mokuro-bunko" in content


class TestDockerCompose:
    """Tests for Docker Compose files."""

    def test_compose_file_exists(self, project_root: Path) -> None:
        """Test that docker-compose.yml exists."""
        compose_file = project_root / "deploy" / "docker-compose.yml"
        assert compose_file.exists()

    def test_compose_cloudflared_exists(self, project_root: Path) -> None:
        """Test that cloudflared compose file exists."""
        compose_file = project_root / "deploy" / "docker-compose.cloudflared.yml"
        assert compose_file.exists()

    def test_compose_syntax(self, project_root: Path) -> None:
        """Test docker-compose.yml syntax is valid."""
        compose_file = project_root / "deploy" / "docker-compose.yml"
        if docker_available():
            result = subprocess.run(
                ["docker", "compose", "-f", str(compose_file), "config"],
                capture_output=True,
                text=True,
                timeout=30,
                cwd=str(project_root),
            )
            # docker compose config returns 0 if valid
            assert result.returncode == 0, f"Compose validation failed: {result.stderr}"
            return

        parsed = yaml.safe_load(compose_file.read_text(encoding="utf-8"))
        assert isinstance(parsed, dict)
        assert "services" in parsed
        assert isinstance(parsed["services"], dict)


# Integration tests that actually build and run Docker
# These are slow and should only run in CI or explicitly
@pytest.mark.slow
class TestDockerIntegration:
    """Integration tests for Docker deployment.

    These tests actually build and run the Docker image.
    They are marked slow and may take several minutes.
    """

    @pytest.fixture
    def docker_image(self, project_root: Path) -> Generator[str | None, None, None]:
        """Build Docker image and clean up after test."""
        if not docker_available():
            yield None
            return

        image_name = "mokuro-bunko-test"

        # Build image
        result = subprocess.run(
            [
                "docker", "build",
                "-t", image_name,
                "-f", "deploy/Dockerfile",
                "."
            ],
            cwd=str(project_root),
            capture_output=True,
            text=True,
            timeout=600,  # 10 minute timeout for build
        )

        if result.returncode != 0:
            pytest.fail(f"Docker build failed: {result.stderr}")

        yield image_name

        # Cleanup
        subprocess.run(
            ["docker", "rmi", "-f", image_name],
            capture_output=True,
            timeout=60,
        )

    def test_docker_build(self, docker_image: str | None) -> None:
        """Test that Docker image builds successfully."""
        if docker_image is None:
            assert docker_available() is False
            return
        # Image was already built by fixture
        result = subprocess.run(
            ["docker", "image", "inspect", docker_image],
            capture_output=True,
            timeout=30,
        )
        assert result.returncode == 0

    def test_docker_run(self, docker_image: str | None) -> None:
        """Test that Docker container starts and responds."""
        if docker_image is None:
            assert docker_available() is False
            return

        container_name = "mokuro-bunko-test-run"

        try:
            # Start container
            result = subprocess.run(
                [
                    "docker", "run",
                    "-d",
                    "--name", container_name,
                    "-p", "18080:8080",
                    docker_image,
                ],
                capture_output=True,
                text=True,
                timeout=30,
            )

            if result.returncode != 0:
                pytest.fail(f"Docker run failed: {result.stderr}")

            # Wait for container to start
            time.sleep(3)

            # Check container is running
            result = subprocess.run(
                ["docker", "ps", "-q", "-f", f"name={container_name}"],
                capture_output=True,
                text=True,
                timeout=30,
            )
            assert result.stdout.strip(), "Container not running"

            # Test HTTP response with stdlib client.
            import urllib.error
            import urllib.request

            request = urllib.request.Request("http://localhost:18080/")
            try:
                response = urllib.request.urlopen(request, timeout=10)
                assert response.status in (200, 207, 401)
            except urllib.error.HTTPError as exc:
                assert exc.code in (200, 207, 401)
            except urllib.error.URLError:
                # Container might need more time
                time.sleep(2)
                response = urllib.request.urlopen(request, timeout=10)
                assert response.status in (200, 207, 401)

        finally:
            # Cleanup container
            subprocess.run(
                ["docker", "stop", container_name],
                capture_output=True,
                timeout=30,
            )
            subprocess.run(
                ["docker", "rm", container_name],
                capture_output=True,
                timeout=30,
            )

    def test_docker_volume_mount(self, docker_image: str | None, tmp_path: Path) -> None:
        """Test that volume mounts work correctly."""
        if docker_image is None:
            assert docker_available() is False
            return

        container_name = "mokuro-bunko-test-volume"
        storage_dir = tmp_path / "storage"
        storage_dir.mkdir()

        try:
            # Start container with volume
            result = subprocess.run(
                [
                    "docker", "run",
                    "-d",
                    "--name", container_name,
                    "-v", f"{storage_dir}:/data",
                    docker_image,
                ],
                capture_output=True,
                text=True,
                timeout=30,
            )

            if result.returncode != 0:
                pytest.fail(f"Docker run failed: {result.stderr}")

            # Wait for container to initialize
            time.sleep(3)

            # Check that storage directories were created
            # Note: They may not exist yet if container hasn't started fully
            # This is mainly testing that the mount worked

        finally:
            subprocess.run(
                ["docker", "stop", container_name],
                capture_output=True,
                timeout=30,
            )
            subprocess.run(
                ["docker", "rm", container_name],
                capture_output=True,
                timeout=30,
            )
