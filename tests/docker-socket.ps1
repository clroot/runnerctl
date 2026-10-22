# Windows counterpart of docker-socket.sh for runnerctl.exe with Docker Desktop. No GitHub
# access: the non-root runner reaches the daemon through the mounted socket, and a sibling
# container sees the runner's workspace at the same VM path, as container jobs require.
$ErrorActionPreference = 'Stop'
$image = if ($env:RUNNER_IMAGE) { $env:RUNNER_IMAGE } else { 'local/runnerctl-test-base:1' }
# Use %LOCALAPPDATA% like the default --home; Docker Desktop shares local drives with its VM.
$root = Join-Path $env:LOCALAPPDATA ('runnerctl-socket-test-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
$runtime = Join-Path $root 'runtime'
# The same mapping runnerctl uses for Docker socket pools: C:\path -> /run/desktop/mnt/host/c/path
$shared = '/run/desktop/mnt/host/' + $runtime.Substring(0, 1).ToLower() + ($runtime.Substring(2) -replace '\\', '/')
try {
    New-Item -ItemType Directory -Path $runtime | Out-Null
    [IO.File]::WriteAllText((Join-Path $root 'token'), 'test-token')
    Copy-Item (Join-Path $PSScriptRoot 'docker-socket-runner.sh') (Join-Path $root 'socket-test.sh')
    docker run --rm --init --network host --security-opt no-new-privileges:true `
        -e GITHUB_ORG=test-org -e RUNNER_NAME=socket-test -e RUNNER_LABELS=test `
        -e "RUNNER_ROOT=$shared" -e RUNNER_UID=0 -e "SIBLING_IMAGE=$image" `
        -v "${root}:/run/runnerctl" -v "${runtime}:${shared}" `
        -v "$(Join-Path $root 'socket-test.sh'):/socket-test.sh:ro" `
        -v /var/run/docker.sock:/var/run/docker.sock `
        --entrypoint /bin/bash $image /socket-test.sh
    if ($LASTEXITCODE -ne 0) { throw "Docker socket test failed with exit code $LASTEXITCODE" }
    if (-not (Test-Path (Join-Path $root 'completed'))) { throw 'completion marker is missing' }
}
finally {
    # Files from containers belong to the Windows user, so no ownership repair is needed.
    if (Test-Path $root) { Remove-Item -Recurse -Force $root }
}
