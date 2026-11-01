Name:           jiffshot
Version:        @VERSION@
Release:        1
Summary:        Screen capture, recording, and non-destructive image editor
License:        LicenseRef-Unknown
URL:            https://github.com/Devolutions/jiffshot
Source0:        payload.tar.gz
BuildArch:      @ARCH@
Requires:       /usr/bin/readlink
Requires:       libX11.so.6()(64bit)
Requires:       libICE.so.6()(64bit)
Requires:       libSM.so.6()(64bit)
Requires:       libXcursor.so.1()(64bit)
Requires:       libXi.so.6()(64bit)
Requires:       libXrandr.so.2()(64bit)
Requires:       gtk3
Requires:       libicu
Requires:       openssl-libs
Requires:       fontconfig
Requires:       dejavu-sans-fonts
Requires:       tesseract-langpack-eng
Requires:       gstreamer1 >= 1.24
Requires:       gstreamer1-plugins-base >= 1.24
Requires:       gstreamer1-plugins-good
Requires:       gstreamer1-plugins-bad-free
Requires:       pipewire-gstreamer
Requires:       desktop-file-utils
Requires:       shared-mime-info
Requires:       hicolor-icon-theme
Recommends:     gstreamer1-plugins-ugly
Recommends:     gstreamer1-plugin-libav

# Prebuilt files are stripped once by the shared payload builder.
%global __os_install_post %{nil}
%global debug_package %{nil}
%global _build_id_links none

%description
Linux image/document editing, OCR, portal capture, and video tools.
Screen capture requires a running XDG desktop portal and PipeWire.
Additional media codecs may require RPM Fusion on Fedora.
The upstream repository does not currently declare an application license.

%prep
%setup -q -n jiffshot-%{version}

%build

%install
mkdir -p %{buildroot}/usr
cp -a bin lib share %{buildroot}/usr/

%files
%attr(0755,root,root) /usr/bin/jiffshot
/usr/lib/jiffshot
/usr/share/applications/com.devolutions.Jiffshot.desktop
/usr/share/icons/hicolor/scalable/apps/com.devolutions.Jiffshot.svg
/usr/share/mime/packages/com.devolutions.Jiffshot.xml
%doc /usr/share/doc/jiffshot
