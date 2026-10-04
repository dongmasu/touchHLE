$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrWhiteSpace($env:TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_ID)) {
    throw 'Set TOUCHHLE_GOOGLE_DESKTOP_OAUTH_CLIENT_ID in the environment before building.'
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$previousPolicyMinimum = [Environment]::GetEnvironmentVariable(
    'CMAKE_POLICY_VERSION_MINIMUM',
    'Process'
)
$buildExitCode = 1

Push-Location $repoRoot
try {
    $env:CMAKE_POLICY_VERSION_MINIMUM = '3.5'
    & cargo build --release
    $buildExitCode = $LASTEXITCODE
}
finally {
    Pop-Location
    if ($null -eq $previousPolicyMinimum) {
        Remove-Item Env:CMAKE_POLICY_VERSION_MINIMUM -ErrorAction SilentlyContinue
    }
    else {
        $env:CMAKE_POLICY_VERSION_MINIMUM = $previousPolicyMinimum
    }
}

if ($buildExitCode -ne 0) {
    throw "cargo build failed with exit code $buildExitCode."
}
