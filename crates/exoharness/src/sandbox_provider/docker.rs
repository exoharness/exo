pub(crate) const DEFAULT_DOCKER_IMAGE: &str = "docker.io/library/ubuntu:24.04";

pub fn default_docker_image() -> String {
    DEFAULT_DOCKER_IMAGE.to_string()
}
