// Native helpers for the PowerShell Linux GUI end-to-end drivers in LinuxE2E.psm1.
using System;
using System.Collections;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.IO.Compression;
using System.Management.Automation;
using System.Reflection;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Microsoft.Win32.SafeHandles;
using Tmds.DBus.Protocol;

namespace JiffshotE2E
{
    public sealed class Variant
    {
        public Variant(string signature, object value)
        {
            Signature = signature;
            Value = value;
        }

        public string Signature { get; }
        public object Value { get; }

        public override string ToString() => $"<{Signature}: {Value}>";
    }

    internal static class Native
    {
        static Native()
        {
            NativeLibrary.SetDllImportResolver(typeof(Native).Assembly, Resolve);
        }

        internal static void EnsureResolver() { }

        private static IntPtr Resolve(string name, Assembly assembly, DllImportSearchPath? path)
        {
            string[] candidates = name switch
            {
                "X11" => new[] { "libX11.so.6", "libX11.so" },
                "Xtst" => new[] { "libXtst.so.6", "libXtst.so" },
                "tesseract" => new[] { "libtesseract.so.5", "libtesseract.so.4", "libtesseract.so" },
                "c" => new[] { "libc.so.6", "libc.so" },
                _ => Array.Empty<string>(),
            };
            foreach (var candidate in candidates)
            {
                if (NativeLibrary.TryLoad(candidate, out var handle))
                {
                    return handle;
                }
            }
            return IntPtr.Zero;
        }
    }

    internal static class Signatures
    {
        internal static int End(string signature, int index)
        {
            switch (signature[index])
            {
                case 'a':
                    return End(signature, index + 1);
                case '(':
                case '{':
                    {
                        char close = signature[index] == '(' ? ')' : '}';
                        int next = index + 1;
                        while (signature[next] != close)
                        {
                            next = End(signature, next);
                        }
                        return next + 1;
                    }
                default:
                    return index + 1;
            }
        }

        internal static List<string> Split(string signature)
        {
            var types = new List<string>();
            int index = 0;
            while (index < (signature?.Length ?? 0))
            {
                int end = End(signature, index);
                types.Add(signature.Substring(index, end - index));
                index = end;
            }
            return types;
        }
    }

    public static class Codec
    {
        public static object[] ReadBody(Message message)
        {
            var reader = message.GetBodyReader();
            var values = new List<object>();
            foreach (var type in Signatures.Split(message.SignatureAsString ?? ""))
            {
                values.Add(Read(ref reader, type));
            }
            return values.ToArray();
        }

        private static object Read(ref Reader reader, string type)
        {
            switch (type[0])
            {
                case 'y': return reader.ReadByte();
                case 'b': return reader.ReadBool();
                case 'n': return reader.ReadInt16();
                case 'q': return reader.ReadUInt16();
                case 'i': return reader.ReadInt32();
                case 'u': return reader.ReadUInt32();
                case 'x': return reader.ReadInt64();
                case 't': return reader.ReadUInt64();
                case 'd': return reader.ReadDouble();
                case 's': return reader.ReadString();
                case 'o': return reader.ReadObjectPathAsString();
                case 'g': return reader.ReadSignatureAsString();
                case 'h': return reader.ReadHandle<SafeFileHandle>();
                case 'v':
                    {
                        string signature = reader.ReadSignatureAsString();
                        return new Variant(signature, Read(ref reader, signature));
                    }
                case '(':
                    {
                        reader.AlignStruct();
                        var fields = new List<object>();
                        foreach (var field in Signatures.Split(type.Substring(1, type.Length - 2)))
                        {
                            fields.Add(Read(ref reader, field));
                        }
                        return fields.ToArray();
                    }
                case 'a':
                    {
                        string element = type.Substring(1);
                        if (element == "y")
                        {
                            return reader.ReadArrayOfByte();
                        }
                        if (element[0] == '{')
                        {
                            var parts = Signatures.Split(element.Substring(1, element.Length - 2));
                            var dictionary = new Dictionary<object, object>();
                            var dictionaryEnd = reader.ReadDictionaryStart();
                            while (reader.HasNext(dictionaryEnd))
                            {
                                reader.AlignStruct();
                                var key = Read(ref reader, parts[0]);
                                dictionary[key] = Read(ref reader, parts[1]);
                            }
                            return dictionary;
                        }
                        var items = new List<object>();
                        var arrayEnd = reader.ReadArrayStart((DBusType)(byte)element[0]);
                        while (reader.HasNext(arrayEnd))
                        {
                            items.Add(Read(ref reader, element));
                        }
                        return items.ToArray();
                    }
                default:
                    throw new NotSupportedException($"unsupported D-Bus type {type}");
            }
        }

        internal static object Unwrap(object value) => value is PSObject wrapped ? wrapped.BaseObject : value;

        private static bool IsObjectPath(string path)
        {
            if (string.IsNullOrEmpty(path) || path[0] != '/')
            {
                return false;
            }
            if (path.Length == 1)
            {
                return true;
            }
            char previous = '/';
            for (int index = 1; index < path.Length; index++)
            {
                char current = path[index];
                bool element = current == '_' || (current >= 'A' && current <= 'Z') || (current >= 'a' && current <= 'z') || (current >= '0' && current <= '9');
                if (!element && !(current == '/' && previous != '/'))
                {
                    return false;
                }
                previous = current;
            }
            return previous != '/';
        }

        public static void WriteBody(ref MessageWriter writer, string signature, object[] body)
        {
            var types = Signatures.Split(signature ?? "");
            body ??= Array.Empty<object>();
            if (types.Count != body.Length)
            {
                throw new ArgumentException($"D-Bus body for {signature} has {body.Length} values");
            }
            for (int index = 0; index < types.Count; index++)
            {
                Write(ref writer, types[index], body[index]);
            }
        }

        private static IEnumerable<object> Items(object value)
        {
            value = Unwrap(value);
            if (value is null)
            {
                yield break;
            }
            if (value is string || value is not IEnumerable enumerable)
            {
                throw new ArgumentException($"expected a D-Bus array or structure, got {value.GetType()}");
            }
            foreach (var item in enumerable)
            {
                yield return item;
            }
        }

        private static void Write(ref MessageWriter writer, string type, object value)
        {
            value = Unwrap(value);
            switch (type[0])
            {
                case 'y': writer.WriteByte(Convert.ToByte(value)); return;
                case 'b': writer.WriteBool(LanguagePrimitives.IsTrue(value)); return;
                case 'n': writer.WriteInt16(Convert.ToInt16(value)); return;
                case 'q': writer.WriteUInt16(Convert.ToUInt16(value)); return;
                case 'i': writer.WriteInt32(Convert.ToInt32(value)); return;
                case 'u': writer.WriteUInt32(Convert.ToUInt32(value)); return;
                case 'x': writer.WriteInt64(Convert.ToInt64(value)); return;
                case 't': writer.WriteUInt64(Convert.ToUInt64(value)); return;
                case 'd': writer.WriteDouble(Convert.ToDouble(value)); return;
                case 's': writer.WriteString(Convert.ToString(value)); return;
                case 'o':
                    {
                        string path = Convert.ToString(value);
                        if (!IsObjectPath(path))
                        {
                            // dbus-daemon disconnects senders of malformed object paths.
                            throw new ArgumentException($"invalid D-Bus object path '{path}'");
                        }
                        writer.WriteObjectPath(path);
                        return;
                    }
                case 'g': writer.WriteSignature(Convert.ToString(value)); return;
                case 'h':
                    if (value is SafeHandle handle)
                    {
                        writer.WriteHandle(handle);
                    }
                    else
                    {
                        writer.WriteHandle(Posix.Duplicate(Convert.ToInt32(value)));
                    }
                    return;
                case 'v':
                    {
                        if (value is not Variant variant)
                        {
                            throw new ArgumentException($"expected a JiffshotE2E.Variant, got {value?.GetType()}");
                        }
                        writer.WriteSignature(variant.Signature);
                        Write(ref writer, variant.Signature, variant.Value);
                        return;
                    }
                case '(':
                    {
                        var fields = Signatures.Split(type.Substring(1, type.Length - 2));
                        var values = new List<object>(Items(value));
                        if (fields.Count != values.Count)
                        {
                            throw new ArgumentException($"structure {type} has {values.Count} values");
                        }
                        writer.WriteStructureStart();
                        for (int index = 0; index < fields.Count; index++)
                        {
                            Write(ref writer, fields[index], values[index]);
                        }
                        return;
                    }
                case 'a':
                    {
                        string element = type.Substring(1);
                        if (element == "y" && value is byte[] bytes)
                        {
                            writer.WriteArray(bytes);
                            return;
                        }
                        if (element[0] == '{')
                        {
                            var parts = Signatures.Split(element.Substring(1, element.Length - 2));
                            var dictionaryStart = writer.WriteDictionaryStart();
                            if (value is IDictionary dictionary)
                            {
                                foreach (DictionaryEntry entry in dictionary)
                                {
                                    writer.WriteDictionaryEntryStart();
                                    Write(ref writer, parts[0], entry.Key);
                                    Write(ref writer, parts[1], entry.Value);
                                }
                            }
                            else if (value is not null)
                            {
                                throw new ArgumentException($"expected a dictionary for {type}");
                            }
                            writer.WriteDictionaryEnd(dictionaryStart);
                            return;
                        }
                        var arrayStart = writer.WriteArrayStart((DBusType)(byte)element[0]);
                        foreach (var item in Items(value))
                        {
                            Write(ref writer, element, item);
                        }
                        writer.WriteArrayEnd(arrayStart);
                        return;
                    }
                default:
                    throw new NotSupportedException($"unsupported D-Bus type {type}");
            }
        }
    }

    /// <summary>A method call received by a mock service; replies may be deferred.</summary>
    public sealed class IncomingCall
    {
        private readonly MethodContext _context;
        private int _finished;

        internal IncomingCall(MethodContext context)
        {
            _context = context;
            var request = context.Request;
            Sender = request.SenderAsString;
            Path = request.PathAsString;
            Interface = request.InterfaceAsString;
            Member = request.MemberAsString;
            Signature = request.SignatureAsString ?? "";
            Body = Codec.ReadBody(request);
        }

        public string Sender { get; }
        public string Path { get; }
        public string Interface { get; }
        public string Member { get; }
        public string Signature { get; }
        public object[] Body { get; }
        public bool Finished => _finished != 0;

        private bool Claim()
        {
            return Interlocked.Exchange(ref _finished, 1) == 0;
        }

        public void Reply(string signature, object[] body)
        {
            if (!Claim())
            {
                throw new InvalidOperationException($"{Interface}.{Member} was already answered");
            }
            try
            {
                if (!_context.NoReplyExpected)
                {
                    // MessageWriter is a ref struct; it must be passed by ref so body writes reach the message.
                    var writer = _context.CreateReplyWriter(string.IsNullOrEmpty(signature) ? null : signature);
                    try
                    {
                        Codec.WriteBody(ref writer, signature, body);
                        _context.Reply(writer.CreateMessage());
                    }
                    finally
                    {
                        writer.Dispose();
                    }
                }
            }
            finally
            {
                _context.Dispose();
            }
        }

        public void Error(string name, string message)
        {
            if (!Claim())
            {
                throw new InvalidOperationException($"{Interface}.{Member} was already answered");
            }
            try
            {
                if (!_context.NoReplyExpected)
                {
                    _context.ReplyError(name, message);
                }
            }
            finally
            {
                _context.Dispose();
            }
        }

        public void UnknownMethod()
        {
            if (!Claim())
            {
                return;
            }
            try
            {
                if (!_context.NoReplyExpected)
                {
                    _context.ReplyUnknownMethodError();
                }
            }
            finally
            {
                _context.Dispose();
            }
        }
    }

    /// <summary>A session-bus connection whose incoming calls are queued for a single-threaded pump.</summary>
    public sealed class Bus : IDisposable
    {
        private readonly DBusConnection _connection;
        private readonly ConcurrentQueue<IncomingCall> _calls = new();

        private Bus(DBusConnection connection)
        {
            _connection = connection;
        }

        public string UniqueName => _connection.UniqueName;

        public static Bus Connect()
        {
            var connection = new DBusConnection(DBusAddress.Session);
            connection.ConnectAsync().AsTask().GetAwaiter().GetResult();
            var bus = new Bus(connection);
            connection.AddMethodHandler(new Handler(bus));
            return bus;
        }

        public void RequestName(string name)
        {
            _connection.RequestNameAsync(name, RequestNameOptions.None).GetAwaiter().GetResult();
        }

        public IncomingCall Next()
        {
            return _calls.TryDequeue(out var call) ? call : null;
        }

        public bool Signal(string path, string iface, string member, string signature, object[] body)
        {
            var writer = _connection.GetMessageWriter();
            writer.WriteSignalHeader(null, path, iface, member, string.IsNullOrEmpty(signature) ? null : signature);
            try
            {
                Codec.WriteBody(ref writer, signature, body);
                return _connection.TrySendMessage(writer.CreateMessage());
            }
            finally
            {
                writer.Dispose();
            }
        }

        public Task<object[]> CallAsync(string destination, string path, string iface, string member, string signature, object[] body)
        {
            var writer = _connection.GetMessageWriter();
            writer.WriteMethodCallHeader(destination, path, iface, member, string.IsNullOrEmpty(signature) ? null : signature);
            try
            {
                Codec.WriteBody(ref writer, signature, body);
                return _connection.CallMethodAsync(writer.CreateMessage(), static (message, _) => Codec.ReadBody(message), null);
            }
            finally
            {
                writer.Dispose();
            }
        }

        public void Dispose()
        {
            _connection.Dispose();
        }

        private sealed class Handler : IPathMethodHandler
        {
            private readonly Bus _bus;

            internal Handler(Bus bus)
            {
                _bus = bus;
            }

            public string Path => "/";
            public bool HandlesChildPaths => true;

            public ValueTask HandleMethodAsync(MethodContext context)
            {
                context.DisposesAsynchronously = true;
                IncomingCall call;
                try
                {
                    call = new IncomingCall(context);
                }
                catch (Exception error)
                {
                    context.ReplyError("org.freedesktop.DBus.Error.InvalidArgs", error.Message);
                    context.Dispose();
                    return default;
                }
                _bus._calls.Enqueue(call);
                return default;
            }
        }
    }

    public static class Posix
    {
        private const int OWronly = 1;
        private const int ORdonly = 0;
        private const int ONonblock = 0x800;
        // Matches Python's non-inheritable os.open so spawned helpers such as xclip never hold a FIFO end open.
        private const int OCloexec = 0x80000;
        private const int FGetfl = 3;
        private const int FSetfl = 4;
        private const int Enxio = 6;
        private const int Eintr = 4;

        static Posix()
        {
            Native.EnsureResolver();
        }

        [DllImport("c", SetLastError = true)] private static extern int kill(int pid, int signal);
        [DllImport("c", SetLastError = true)] private static extern int mkfifo(string path, uint mode);
        [DllImport("c", SetLastError = true)] private static extern int open(string path, int flags, uint mode);
        [DllImport("c", SetLastError = true)] private static extern int close(int fd);
        [DllImport("c", SetLastError = true)] private static extern int fcntl(int fd, int command, int argument);
        [DllImport("c", SetLastError = true)] private static extern nint write(int fd, byte[] buffer, nint count);
        [DllImport("c", SetLastError = true)] private static extern int pipe(int[] fds);
        [DllImport("c", SetLastError = true)] private static extern int dup(int fd);
        [DllImport("c", SetLastError = true)] private static extern int flock(int fd, int operation);
        [DllImport("c", SetLastError = true)] private static extern int link(string existing, string created);
        [DllImport("c", SetLastError = true)] private static extern int chmod(string path, uint mode);

        private static IOException Failure(string operation)
        {
            int errno = Marshal.GetLastPInvokeError();
            return new IOException($"{operation} failed: {Marshal.GetPInvokeErrorMessage(errno)} (errno {errno})");
        }

        public static void Kill(int pid, int signal)
        {
            if (kill(pid, signal) != 0)
            {
                throw Failure($"kill({pid}, {signal})");
            }
        }

        public static void MakeFifo(string path)
        {
            if (mkfifo(path, Convert.ToUInt32("600", 8)) != 0)
            {
                throw Failure($"mkfifo({path})");
            }
        }

        public static void Chmod(string path, int mode)
        {
            if (chmod(path, (uint)mode) != 0)
            {
                throw Failure($"chmod({path})");
            }
        }

        /// <summary>Writes a FIFO only when a reader is waiting; returns false while no reader has opened it.</summary>
        public static bool SendFifo(string path, byte[] payload)
        {
            int fd = TryOpenFifoWriter(path);
            if (fd < 0)
            {
                return false;
            }
            try
            {
                WriteAll(fd, payload);
            }
            finally
            {
                close(fd);
            }
            return true;
        }

        /// <summary>Opens a blocking FIFO writer once a reader is waiting; returns -1 while no reader has opened it.</summary>
        public static int TryOpenFifoWriter(string path)
        {
            int fd = open(path, OWronly | ONonblock | OCloexec, 0);
            if (fd < 0)
            {
                int errno = Marshal.GetLastPInvokeError();
                if (errno == Enxio)
                {
                    return -1;
                }
                throw new IOException($"open({path}) failed: {Marshal.GetPInvokeErrorMessage(errno)}");
            }
            int flags = fcntl(fd, FGetfl, 0);
            if (flags < 0 || fcntl(fd, FSetfl, flags & ~ONonblock) < 0)
            {
                var failure = Failure("fcntl");
                close(fd);
                throw failure;
            }
            return fd;
        }

        public static void WriteAll(int fd, byte[] payload)
        {
            int offset = 0;
            while (offset < payload.Length)
            {
                var chunk = offset == 0 ? payload : payload[offset..];
                nint written = write(fd, chunk, chunk.Length);
                if (written < 0)
                {
                    if (Marshal.GetLastPInvokeError() == Eintr)
                    {
                        continue;
                    }
                    throw Failure("write");
                }
                offset += (int)written;
            }
        }

        /// <summary>Creates a pipe and returns its read and write descriptors.</summary>
        public static int[] Pipe()
        {
            var fds = new int[2];
            if (pipe(fds) != 0)
            {
                throw Failure("pipe");
            }
            return fds;
        }

        public static void Close(int fd)
        {
            if (close(fd) != 0)
            {
                throw Failure($"close({fd})");
            }
        }

        public static SafeFileHandle Duplicate(int fd)
        {
            int copy = dup(fd);
            if (copy < 0)
            {
                throw Failure($"dup({fd})");
            }
            return new SafeFileHandle((IntPtr)copy, true);
        }

        /// <summary>Opens a file and takes a non-blocking exclusive flock; returns the descriptor or -1 when it is held elsewhere.</summary>
        public static int TryLockExclusive(string path)
        {
            int fd = open(path, ORdonly | OCloexec, 0);
            if (fd < 0)
            {
                throw Failure($"open({path})");
            }
            if (flock(fd, 2 | 4) != 0)
            {
                close(fd);
                return -1;
            }
            return fd;
        }

        public static void Link(string existing, string created)
        {
            if (link(existing, created) != 0)
            {
                throw Failure($"link({existing}, {created})");
            }
        }
    }

    /// <summary>A child process started through /bin/sh so redirections match subprocess semantics.</summary>
    public sealed class ChildProcess : IDisposable
    {
        private readonly Process _process;
        private readonly List<string> _lines;
        private readonly Task _drain;

        private ChildProcess(Process process, Task drain, List<string> lines)
        {
            _process = process;
            _drain = drain;
            _lines = lines;
        }

        public bool OutputClosed => _drain.IsCompleted;

        public int Id => _process.Id;
        public bool HasExited => _process.HasExited;
        public int ExitCode => _process.ExitCode;
        public Process Process => _process;

        private static string Quote(string value) => "'" + value.Replace("'", "'\"'\"'") + "'";

        private static ProcessStartInfo Info(IDictionary environment, string workingDirectory)
        {
            var info = new ProcessStartInfo("/bin/sh")
            {
                UseShellExecute = false,
                WorkingDirectory = workingDirectory ?? Environment.CurrentDirectory,
            };
            if (environment is not null)
            {
                info.Environment.Clear();
                foreach (DictionaryEntry entry in environment)
                {
                    if (entry.Value is not null)
                    {
                        info.Environment[Convert.ToString(entry.Key)] = Convert.ToString(Codec.Unwrap(entry.Value));
                    }
                }
            }
            return info;
        }

        /// <summary>Starts argv with stdin from /dev/null and stdout/stderr appended to logPath.</summary>
        public static ChildProcess Start(string[] argv, IDictionary environment, string workingDirectory, string logPath)
        {
            var info = Info(environment, workingDirectory);
            var command = new StringBuilder("exec");
            foreach (var argument in argv)
            {
                command.Append(' ').Append(Quote(argument));
            }
            command.Append(" </dev/null");
            if (logPath is not null)
            {
                command.Append(" >>").Append(Quote(logPath)).Append(" 2>&1");
            }
            info.ArgumentList.Add("-c");
            info.ArgumentList.Add(command.ToString());
            var process = Process.Start(info) ?? throw new InvalidOperationException($"could not start {argv[0]}");
            return new ChildProcess(process, Task.CompletedTask, new List<string>());
        }

        /// <summary>Starts argv capturing stdout lines; stderr is appended to logPath.</summary>
        public static ChildProcess StartCapturing(string[] argv, IDictionary environment, string workingDirectory, string logPath)
        {
            var info = Info(environment, workingDirectory);
            var command = new StringBuilder("exec");
            foreach (var argument in argv)
            {
                command.Append(' ').Append(Quote(argument));
            }
            command.Append(" </dev/null");
            if (logPath is not null)
            {
                command.Append(" 2>>").Append(Quote(logPath));
            }
            info.ArgumentList.Add("-c");
            info.ArgumentList.Add(command.ToString());
            info.RedirectStandardOutput = true;
            var process = Process.Start(info) ?? throw new InvalidOperationException($"could not start {argv[0]}");
            var lines = new List<string>();
            var drain = Task.Run(() =>
            {
                string line;
                while ((line = process.StandardOutput.ReadLine()) is not null)
                {
                    lock (lines)
                    {
                        lines.Add(line);
                    }
                }
            });
            return new ChildProcess(process, drain, lines);
        }

        public string[] Lines()
        {
            lock (_lines)
            {
                return _lines.ToArray();
            }
        }

        public bool WaitForExit(double seconds)
        {
            return _process.WaitForExit(TimeSpan.FromSeconds(seconds));
        }

        /// <summary>Sends SIGTERM, waits five seconds, then kills.</summary>
        public void Stop()
        {
            if (_process.HasExited)
            {
                return;
            }
            try
            {
                Posix.Kill(_process.Id, 15);
            }
            catch (IOException)
            {
            }
            if (!_process.WaitForExit(5000))
            {
                _process.Kill();
                _process.WaitForExit(5000);
            }
        }

        public void Dispose()
        {
            Stop();
            _process.Dispose();
        }
    }

    public sealed class Frame
    {
        public Frame(int width, int height, int stride, byte[] data)
        {
            Width = width;
            Height = height;
            Stride = stride;
            Data = data;
        }

        public int Width { get; }
        public int Height { get; }
        public int Stride { get; }
        /// <summary>X11 ZPixmap bytes: blue, green, red, padding.</summary>
        public byte[] Data { get; }

        public int[] Pixel(int x, int y)
        {
            int offset = y * Stride + x * 4;
            return new[] { (int)Data[offset + 2], Data[offset + 1], Data[offset] };
        }

        public bool SameAs(Frame other)
        {
            return other is not null && Stride == other.Stride && Data.AsSpan().SequenceEqual(other.Data);
        }

        public string Digest() => Convert.ToHexString(SHA256.HashData(Data));

        public int CountRed()
        {
            int count = 0;
            for (int y = 0; y < Height; y++)
            {
                for (int x = 0; x < Width; x++)
                {
                    int offset = y * Stride + x * 4;
                    if (Data[offset + 2] > 170 && Data[offset + 1] < 100 && Data[offset] < 100)
                    {
                        count++;
                    }
                }
            }
            return count;
        }

        public int CountColor(int red, int green, int blue)
        {
            int count = 0;
            for (int y = 0; y < Height; y++)
            {
                for (int x = 0; x < Width; x++)
                {
                    int offset = y * Stride + x * 4;
                    if (Data[offset + 2] == red && Data[offset + 1] == green && Data[offset] == blue)
                    {
                        count++;
                    }
                }
            }
            return count;
        }

        /// <summary>Counts pixels whose channels each lie within tolerance of the color.</summary>
        public int CountNear(int red, int green, int blue, int tolerance)
        {
            int count = 0;
            for (int y = 0; y < Height; y++)
            {
                for (int x = 0; x < Width; x++)
                {
                    int offset = y * Stride + x * 4;
                    if (Math.Abs(Data[offset + 2] - red) <= tolerance
                        && Math.Abs(Data[offset + 1] - green) <= tolerance
                        && Math.Abs(Data[offset] - blue) <= tolerance)
                    {
                        count++;
                    }
                }
            }
            return count;
        }

        public byte[] Grayscale(int scale)
        {
            int width = Width * scale;
            var gray = new byte[width * Height * scale];
            for (int y = 0; y < Height; y++)
            {
                for (int x = 0; x < Width; x++)
                {
                    int offset = y * Stride + x * 4;
                    byte value = (byte)((Data[offset] + Data[offset + 1] + Data[offset + 2]) / 3);
                    for (int dy = 0; dy < scale; dy++)
                    {
                        int row = (y * scale + dy) * width + x * scale;
                        for (int dx = 0; dx < scale; dx++)
                        {
                            gray[row + dx] = value;
                        }
                    }
                }
            }
            return gray;
        }

        public byte[] ToPng()
        {
            var rows = new byte[Height * (Width * 3 + 1)];
            int target = 0;
            for (int y = 0; y < Height; y++)
            {
                rows[target++] = 0;
                for (int x = 0; x < Width; x++)
                {
                    int offset = y * Stride + x * 4;
                    rows[target++] = Data[offset + 2];
                    rows[target++] = Data[offset + 1];
                    rows[target++] = Data[offset];
                }
            }
            return Png.Encode(Width, Height, 2, rows);
        }
    }

    public sealed class Image
    {
        public Image(int width, int height, byte[] rgba)
        {
            Width = width;
            Height = height;
            Rgba = rgba;
        }

        public int Width { get; }
        public int Height { get; }
        public byte[] Rgba { get; }

        public int[] Pixel(int x, int y)
        {
            int offset = (y * Width + x) * 4;
            return new int[] { Rgba[offset], Rgba[offset + 1], Rgba[offset + 2], Rgba[offset + 3] };
        }

        public bool SameAs(Image other)
        {
            return other is not null && Width == other.Width && Height == other.Height
                && Rgba.AsSpan().SequenceEqual(other.Rgba);
        }

        public int CountColor(int red, int green, int blue, int alpha)
        {
            int count = 0;
            for (int offset = 0; offset < Rgba.Length; offset += 4)
            {
                if (Rgba[offset] == red && Rgba[offset + 1] == green && Rgba[offset + 2] == blue && Rgba[offset + 3] == alpha)
                {
                    count++;
                }
            }
            return count;
        }
    }

    public static class Png
    {
        private static readonly uint[] CrcTable = BuildCrcTable();

        private static uint[] BuildCrcTable()
        {
            var table = new uint[256];
            for (uint n = 0; n < 256; n++)
            {
                uint c = n;
                for (int k = 0; k < 8; k++)
                {
                    c = (c & 1) != 0 ? 0xEDB88320u ^ (c >> 1) : c >> 1;
                }
                table[n] = c;
            }
            return table;
        }

        public static uint Crc32(byte[] data)
        {
            uint c = 0xFFFFFFFFu;
            foreach (byte b in data)
            {
                c = CrcTable[(c ^ b) & 0xFF] ^ (c >> 8);
            }
            return c ^ 0xFFFFFFFFu;
        }

        private static void BigEndian(Stream stream, uint value)
        {
            stream.WriteByte((byte)(value >> 24));
            stream.WriteByte((byte)(value >> 16));
            stream.WriteByte((byte)(value >> 8));
            stream.WriteByte((byte)value);
        }

        private static void Chunk(Stream stream, string kind, byte[] data)
        {
            BigEndian(stream, (uint)data.Length);
            var typed = new byte[4 + data.Length];
            Encoding.ASCII.GetBytes(kind, 0, 4, typed, 0);
            data.CopyTo(typed, 4);
            stream.Write(typed);
            BigEndian(stream, Crc32(typed));
        }

        public static byte[] Compress(byte[] data)
        {
            using var output = new MemoryStream();
            using (var zlib = new ZLibStream(output, CompressionLevel.Optimal, true))
            {
                zlib.Write(data);
            }
            return output.ToArray();
        }

        /// <summary>Encodes 8-bit filtered scanlines (filter byte per row already present).</summary>
        public static byte[] Encode(int width, int height, int colorType, byte[] filteredRows)
        {
            using var png = new MemoryStream();
            png.Write(new byte[] { 0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A });
            using (var header = new MemoryStream())
            {
                BigEndian(header, (uint)width);
                BigEndian(header, (uint)height);
                header.Write(new byte[] { 8, (byte)colorType, 0, 0, 0 });
                Chunk(png, "IHDR", header.ToArray());
            }
            Chunk(png, "IDAT", Compress(filteredRows));
            Chunk(png, "IEND", Array.Empty<byte>());
            return png.ToArray();
        }

        public static byte[] Solid(int width, int height, int red, int green, int blue)
        {
            var rows = new byte[height * (width * 3 + 1)];
            int offset = 0;
            for (int y = 0; y < height; y++)
            {
                rows[offset++] = 0;
                for (int x = 0; x < width; x++)
                {
                    rows[offset++] = (byte)red;
                    rows[offset++] = (byte)green;
                    rows[offset++] = (byte)blue;
                }
            }
            return Encode(width, height, 2, rows);
        }

        private static uint ReadBigEndian(byte[] data, int offset)
        {
            return (uint)(data[offset] << 24 | data[offset + 1] << 16 | data[offset + 2] << 8 | data[offset + 3]);
        }

        public static Image Decode(byte[] data)
        {
            byte[] signature = { 0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A };
            if (data.Length < 8 || !data.AsSpan(0, 8).SequenceEqual(signature))
            {
                throw new InvalidDataException("export must be a PNG");
            }
            int offset = 8;
            int width = 0, height = 0, channels = 0;
            using var compressed = new MemoryStream();
            while (offset < data.Length)
            {
                int length = (int)ReadBigEndian(data, offset);
                string kind = Encoding.ASCII.GetString(data, offset + 4, 4);
                int start = offset + 8;
                offset += length + 12;
                if (kind == "IHDR")
                {
                    width = (int)ReadBigEndian(data, start);
                    height = (int)ReadBigEndian(data, start + 4);
                    byte depth = data[start + 8], color = data[start + 9];
                    if (depth != 8 || (color != 2 && color != 6))
                    {
                        throw new InvalidDataException($"unsupported PNG depth {depth} color {color}");
                    }
                    if (data[start + 10] != 0 || data[start + 11] != 0 || data[start + 12] != 0)
                    {
                        throw new InvalidDataException("unsupported PNG compression, filter, or interlace");
                    }
                    channels = color == 2 ? 3 : 4;
                }
                else if (kind == "IDAT")
                {
                    compressed.Write(data, start, length);
                }
                else if (kind == "IEND")
                {
                    break;
                }
            }
            if (width == 0 || height == 0 || channels == 0)
            {
                throw new InvalidDataException("PNG header must describe an image");
            }
            compressed.Position = 0;
            using var inflated = new MemoryStream();
            using (var zlib = new ZLibStream(compressed, CompressionMode.Decompress))
            {
                zlib.CopyTo(inflated);
            }
            var raw = inflated.ToArray();
            int stride = width * channels;
            if (raw.Length != height * (stride + 1))
            {
                throw new InvalidDataException("PNG must contain every scanline");
            }
            var previous = new byte[stride];
            var row = new byte[stride];
            var rgba = new byte[width * height * 4];
            int target = 0;
            for (int y = 0; y < height; y++)
            {
                int rowStart = y * (stride + 1);
                byte mode = raw[rowStart];
                if (mode > 4)
                {
                    throw new InvalidDataException($"unsupported PNG filter {mode}");
                }
                Buffer.BlockCopy(raw, rowStart + 1, row, 0, stride);
                for (int i = 0; i < stride; i++)
                {
                    int left = i >= channels ? row[i - channels] : 0;
                    int above = previous[i];
                    int upperLeft = i >= channels ? previous[i - channels] : 0;
                    int predictor = mode switch
                    {
                        1 => left,
                        2 => above,
                        3 => (left + above) / 2,
                        4 => Paeth(left, above, upperLeft),
                        _ => 0,
                    };
                    row[i] = (byte)(row[i] + predictor);
                }
                for (int i = 0; i < stride; i += channels)
                {
                    rgba[target++] = row[i];
                    rgba[target++] = row[i + 1];
                    rgba[target++] = row[i + 2];
                    rgba[target++] = channels == 4 ? row[i + 3] : (byte)255;
                }
                (previous, row) = (row, previous);
            }
            return new Image(width, height, rgba);
        }

        private static int Paeth(int left, int above, int upperLeft)
        {
            int estimate = left + above - upperLeft;
            int a = Math.Abs(estimate - left), b = Math.Abs(estimate - above), c = Math.Abs(estimate - upperLeft);
            if (a <= b && a <= c)
            {
                return left;
            }
            return b <= c ? above : upperLeft;
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct XImage
    {
        public int width, height, xoffset, format;
        public IntPtr data;
        public int byte_order, bitmap_unit, bitmap_bit_order, bitmap_pad, depth, bytes_per_line, bits_per_pixel;
        public nuint red_mask, green_mask, blue_mask;
    }

    [StructLayout(LayoutKind.Explicit, Size = 192)]
    internal struct XClientMessageEvent
    {
        [FieldOffset(0)] public int type;
        [FieldOffset(8)] public nuint serial;
        [FieldOffset(16)] public int send_event;
        [FieldOffset(24)] public IntPtr display;
        [FieldOffset(32)] public nuint window;
        [FieldOffset(40)] public nuint message_type;
        [FieldOffset(48)] public int format;
        [FieldOffset(56)] public nint l0;
        [FieldOffset(64)] public nint l1;
        [FieldOffset(72)] public nint l2;
        [FieldOffset(80)] public nint l3;
        [FieldOffset(88)] public nint l4;
    }

    public sealed class X11Exception : Exception
    {
        public X11Exception(string message) : base(message) { }
    }

    /// <summary>Drives a private X11 window through XTest and captures its pixels.</summary>
    public sealed class X11Driver : IDisposable
    {
        private delegate int ErrorHandler(IntPtr display, IntPtr error);

        private static readonly ConcurrentQueue<(IntPtr Display, string Message)> Errors = new();
        private static readonly ErrorHandler KeepAlive = RecordError;
        private static int _installed;

        [DllImport("X11")] private static extern IntPtr XOpenDisplay(string name);
        [DllImport("X11")] private static extern int XDefaultScreen(IntPtr display);
        [DllImport("X11")] private static extern int XSetInputFocus(IntPtr display, nuint window, int revert, nuint time);
        [DllImport("X11")] private static extern int XResizeWindow(IntPtr display, nuint window, uint width, uint height);
        [DllImport("X11")] private static extern nuint XStringToKeysym(string name);
        [DllImport("X11")] private static extern byte XKeysymToKeycode(IntPtr display, nuint keysym);
        [DllImport("X11")] private static extern int XSync(IntPtr display, int discard);
        [DllImport("X11")] private static extern int XCloseDisplay(IntPtr display);
        [DllImport("X11")] private static extern IntPtr XGetImage(IntPtr display, nuint drawable, int x, int y, uint width, uint height, nuint planes, int format);
        [DllImport("X11")] private static extern int XDestroyImage(IntPtr image);
        [DllImport("X11")] private static extern nuint XInternAtom(IntPtr display, string name, int onlyIfExists);
        [DllImport("X11")] private static extern int XSendEvent(IntPtr display, nuint window, int propagate, nint mask, ref XClientMessageEvent ev);
        [DllImport("X11")] private static extern IntPtr XSetErrorHandler(ErrorHandler handler);
        [DllImport("X11")] private static extern int XGetErrorText(IntPtr display, int code, byte[] buffer, int length);
        [DllImport("Xtst")] private static extern int XTestFakeMotionEvent(IntPtr display, int screen, int x, int y, nuint delay);
        [DllImport("Xtst")] private static extern int XTestFakeButtonEvent(IntPtr display, uint button, int press, nuint delay);
        [DllImport("Xtst")] private static extern int XTestFakeKeyEvent(IntPtr display, uint keycode, int press, nuint delay);

        static X11Driver()
        {
            Native.EnsureResolver();
        }

        private static int RecordError(IntPtr display, IntPtr error)
        {
            // XErrorEvent: type, display, resourceid, serial, error_code, request_code, minor_code.
            nuint resource = (nuint)Marshal.ReadIntPtr(error, 16);
            byte code = Marshal.ReadByte(error, 32);
            byte request = Marshal.ReadByte(error, 33);
            var text = new byte[256];
            XGetErrorText(display, code, text, text.Length);
            string description = Encoding.UTF8.GetString(text).TrimEnd('\0');
            Errors.Enqueue((display, $"X Error of failed request: {description} (request {request}, resource 0x{resource:x})"));
            return 0;
        }

        public X11Driver(string displayName, long window, int originX, int originY)
        {
            if (Interlocked.Exchange(ref _installed, 1) == 0)
            {
                XSetErrorHandler(KeepAlive);
            }
            Display = XOpenDisplay(displayName);
            if (Display == IntPtr.Zero)
            {
                throw new X11Exception($"could not open private display {displayName}");
            }
            Screen = XDefaultScreen(Display);
            Window = window;
            OriginX = originX;
            OriginY = originY;
            try
            {
                XSetInputFocus(Display, (nuint)window, 2, 0);
                XResizeWindow(Display, (nuint)window, 800, 500);
                Sync();
            }
            catch
            {
                XCloseDisplay(Display);
                Display = IntPtr.Zero;
                throw;
            }
        }

        public IntPtr Display { get; private set; }
        public int Screen { get; }
        public long Window { get; }
        public int OriginX { get; }
        public int OriginY { get; }

        /// <summary>Flushes requests and throws if the X server reported an error for this display.</summary>
        public void Sync()
        {
            XSync(Display, 0);
            ThrowErrors();
        }

        private void ThrowErrors()
        {
            var messages = new List<string>();
            var others = new List<(IntPtr, string)>();
            while (Errors.TryDequeue(out var error))
            {
                if (error.Display == Display)
                {
                    messages.Add(error.Message);
                }
                else
                {
                    others.Add(error);
                }
            }
            foreach (var other in others)
            {
                Errors.Enqueue(other);
            }
            if (messages.Count > 0)
            {
                throw new X11Exception(string.Join("; ", messages));
            }
        }

        public void Move(int x, int y)
        {
            XTestFakeMotionEvent(Display, Screen, x + OriginX, y + OriginY, 0);
        }

        public void Button(int button, bool down)
        {
            XTestFakeButtonEvent(Display, (uint)button, down ? 1 : 0, 0);
        }

        public void Click(int x, int y)
        {
            Move(x, y);
            Button(1, true);
            Button(1, false);
            Sync();
        }

        public void Key(string name, bool down)
        {
            nuint keysym = XStringToKeysym(name);
            byte code = XKeysymToKeycode(Display, keysym);
            if (code == 0)
            {
                throw new X11Exception($"no X11 keycode for '{name}'");
            }
            XTestFakeKeyEvent(Display, code, down ? 1 : 0, 0);
        }

        public void Press(string name)
        {
            Key(name, true);
            Key(name, false);
            Sync();
        }

        public void SelectAll()
        {
            Key("Control_L", true);
            Key("a", true);
            Key("a", false);
            Key("Control_L", false);
            Sync();
        }

        /// <summary>Moves to (x, y) in window coordinates and clicks the wheel button count times.</summary>
        public void Wheel(int x, int y, int button, int count)
        {
            Move(x, y);
            for (int index = 0; index < count; index++)
            {
                Button(button, true);
                Button(button, false);
            }
            Sync();
        }

        public Frame Capture(int x, int y, int width, int height)
        {
            IntPtr pointer = XGetImage(Display, (nuint)Window, x, y, (uint)width, (uint)height, nuint.MaxValue, 2);
            if (pointer == IntPtr.Zero)
            {
                var detail = new List<string>();
                while (Errors.TryDequeue(out var error))
                {
                    detail.Add(error.Message);
                }
                throw new X11Exception("could not read the app window" + (detail.Count > 0 ? ": " + string.Join("; ", detail) : ""));
            }
            try
            {
                var image = Marshal.PtrToStructure<XImage>(pointer);
                if (image.bits_per_pixel != 32 || image.byte_order != 0 || image.red_mask != 0xFF0000
                    || image.green_mask != 0xFF00 || image.blue_mask != 0xFF)
                {
                    throw new X11Exception("unexpected X11 pixel format");
                }
                var data = new byte[image.bytes_per_line * image.height];
                Marshal.Copy(image.data, data, 0, data.Length);
                return new Frame(image.width, image.height, image.bytes_per_line, data);
            }
            finally
            {
                XDestroyImage(pointer);
            }
        }

        public int RedCanvasPixels() => Capture(0, 280, 800, 190).CountRed();

        /// <summary>Asks the window manager protocol to close the window, like a title-bar close button.</summary>
        public void RequestClose()
        {
            var message = new XClientMessageEvent
            {
                type = 33,
                display = Display,
                window = (nuint)Window,
                message_type = XInternAtom(Display, "WM_PROTOCOLS", 0),
                format = 32,
                l0 = (nint)XInternAtom(Display, "WM_DELETE_WINDOW", 0),
            };
            if (XSendEvent(Display, (nuint)Window, 0, 0, ref message) == 0)
            {
                throw new X11Exception("could not send the window close request");
            }
            Sync();
        }

        public void Dispose()
        {
            if (Display != IntPtr.Zero)
            {
                XCloseDisplay(Display);
                Display = IntPtr.Zero;
            }
        }
    }

    /// <summary>Reads visible UI text with the installed Tesseract library.</summary>
    public sealed class TextReader : IDisposable
    {
        [DllImport("tesseract")] private static extern IntPtr TessBaseAPICreate();
        [DllImport("tesseract")] private static extern int TessBaseAPIInit3(IntPtr api, string dataPath, string language);
        [DllImport("tesseract")] private static extern void TessBaseAPISetPageSegMode(IntPtr api, int mode);
        [DllImport("tesseract")] private static extern void TessBaseAPISetImage(IntPtr api, byte[] data, int width, int height, int bytesPerPixel, int bytesPerLine);
        [DllImport("tesseract")] private static extern int TessBaseAPIRecognize(IntPtr api, IntPtr monitor);
        [DllImport("tesseract")] private static extern IntPtr TessBaseAPIGetUTF8Text(IntPtr api);
        [DllImport("tesseract")] private static extern IntPtr TessBaseAPIGetTsvText(IntPtr api, int page);
        [DllImport("tesseract")] private static extern void TessDeleteText(IntPtr text);
        [DllImport("tesseract")] private static extern void TessBaseAPIDelete(IntPtr api);

        private IntPtr _api;

        static TextReader()
        {
            Native.EnsureResolver();
        }

        public TextReader()
        {
            try
            {
                _api = TessBaseAPICreate();
            }
            catch (DllNotFoundException error)
            {
                throw new InvalidOperationException("Tesseract is required to verify visible UI text", error);
            }
            if (_api == IntPtr.Zero || TessBaseAPIInit3(_api, null, "eng") != 0)
            {
                throw new InvalidOperationException("could not initialize UI-text OCR");
            }
        }

        public string Recognize(Frame frame, int pageMode, bool tsv)
        {
            const int scale = 3;
            var gray = frame.Grayscale(scale);
            TessBaseAPISetPageSegMode(_api, pageMode);
            TessBaseAPISetImage(_api, gray, frame.Width * scale, frame.Height * scale, 1, frame.Width * scale);
            if (TessBaseAPIRecognize(_api, IntPtr.Zero) != 0)
            {
                throw new InvalidOperationException("UI-text OCR failed");
            }
            IntPtr result = tsv ? TessBaseAPIGetTsvText(_api, 0) : TessBaseAPIGetUTF8Text(_api);
            if (result == IntPtr.Zero)
            {
                throw new InvalidOperationException("UI-text OCR returned no text");
            }
            try
            {
                return Marshal.PtrToStringUTF8(result);
            }
            finally
            {
                TessDeleteText(result);
            }
        }

        public void Dispose()
        {
            if (_api != IntPtr.Zero)
            {
                TessBaseAPIDelete(_api);
                _api = IntPtr.Zero;
            }
        }
    }
}
