# SWE-bench publishes one prebuilt image per instance with the repository
# checked out at the base commit under /testbed and its conda env installed.
FROM {docker_image}

WORKDIR /testbed

# The verifier's grading script runs under uv.
RUN curl -LsSf https://astral.sh/uv/0.7.13/install.sh | sh
RUN mkdir -p /logs
