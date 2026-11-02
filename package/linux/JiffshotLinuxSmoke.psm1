function Find-JiffshotSmokeWindowId {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)][AllowEmptyString()][string]$WindowTree,
        [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$ImageName
    )

    $pattern = '(?m)^\s*(?<id>0x[0-9a-fA-F]+)\s+"' +
        [regex]::Escape($ImageName) + ' [^\s"\r\n]+ Jiffshot Editor":'
    $match = [regex]::Match($WindowTree, $pattern)
    if ($match.Success) {
        $match.Groups['id'].Value
    }
}

Export-ModuleMember -Function Find-JiffshotSmokeWindowId
