param(
    [string] $FixedArgument,
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $RequestArguments
)

$ErrorActionPreference = "Stop"

Write-Output "fixed argument: $FixedArgument"
for ($Index = 0; $Index -lt $RequestArguments.Count; $Index++) {
    Write-Output "request argument $($Index + 1): $($RequestArguments[$Index])"
}

[Console]::Error.WriteLine("example stderr line")
