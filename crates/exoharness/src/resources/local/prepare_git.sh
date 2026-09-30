umask 077
url=$1
kind=$2
reference=$3
depth=$4
g() {
  git -c safe.directory="$PWD" -c protocol.allow=never -c protocol.https.allow=always \
    -c core.hooksPath=/dev/null -c core.fsmonitor=false \
    -c submodule.recurse=false -c fetch.recurseSubmodules=false \
    -c filter.lfs.required=false -c filter.lfs.smudge= \
    -c filter.lfs.process= -c http.followRedirects=false "$@"
}
if [ ! -d .git ]; then
  g init --quiet
  g remote add origin "$url"
fi
g remote set-url origin "$url"
if [ "$kind" = default ]; then
  reference=$(g ls-remote --symref origin HEAD | sed -n 's/^ref: refs\/heads\/\(.*\)[[:space:]]HEAD$/\1/p')
  test -n "$reference"
  kind=branch
fi
if [ "$kind" = branch ]; then
  g check-ref-format "refs/heads/$reference"
  refspec="+refs/heads/$reference:refs/remotes/origin/$reference"
  g fetch --quiet --force --no-tags --no-recurse-submodules --depth="$depth" origin "$refspec"
  g checkout --quiet --force -B "$reference" "refs/remotes/origin/$reference"
  g config "branch.$reference.remote" origin
  g config "branch.$reference.merge" "refs/heads/$reference"
  g config remote.origin.fetch "$refspec"
else
  g fetch --quiet --force --no-tags --no-recurse-submodules --depth="$depth" origin "$reference"
  g checkout --quiet --force --detach FETCH_HEAD
fi
g clean -ffd --quiet
g rev-parse HEAD
