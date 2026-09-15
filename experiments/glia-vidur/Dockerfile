FROM python:3.11.15-slim-bookworm@sha256:d29f48a31a8b408ed19272ca1e7b10ebae13b240a27e862d3d4217c528e2e0c3
ENV PYTHONUNBUFFERED=1 PYTHONDONTWRITEBYTECODE=1 WANDB_MODE=disabled \
    OPENBLAS_NUM_THREADS=1 OMP_NUM_THREADS=1 MPLCONFIGDIR=/tmp/matplotlib
COPY requirements.lock /opt/experiment/requirements.lock
RUN pip install --no-cache-dir -r /opt/experiment/requirements.lock
COPY .work/vidur/vidur /opt/vidur/vidur
COPY .work/vidur/data /opt/vidur/data
COPY .work/vidur/LICENSE /opt/vidur/LICENSE
COPY .work/vidur-revision /opt/vidur/REVISION
COPY benchmark.py evaluate.py /opt/experiment/
ENV PYTHONPATH=/opt/vidur
WORKDIR /workspace
CMD ["bash"]
