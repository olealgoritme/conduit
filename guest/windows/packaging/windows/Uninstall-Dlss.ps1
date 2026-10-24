<#
.SYNOPSIS
  Undo Install-Dlss.ps1: restore the registry values and files it changed.
  Run from an elevated PowerShell; close the game first.
#>
[CmdletBinding()]
param()
& (Join-Path $PSScriptRoot 'Install-Dlss.ps1') -Uninstall
