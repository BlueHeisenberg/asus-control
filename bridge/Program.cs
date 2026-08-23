using System.Text;
using System.Text.Json;
using LibreHardwareMonitor.Hardware;

namespace SensorBridge;

internal static class Program
{
    private sealed class SensorInfo
    {
        public required IHardware Hardware { get; init; }
        public required ISensor Sensor { get; init; }
        public required string Id { get; init; }
        public required string Name { get; init; }
        public required string Type { get; init; }
        public required string Unit { get; init; }
    }

    private sealed class HardwareInfo
    {
        public required string Id { get; init; }
        public required string Name { get; init; }
        public required string Type { get; init; }
        public List<SensorInfo> Sensors { get; } = new();
    }

    private static Computer? _computer;

    private static void Main()
    {
        try
        {
            Console.OutputEncoding = Encoding.UTF8;
        }
        catch
        {
            // Non-fatal: keep default encoding if stdout is not writable yet.
        }

        _computer = new Computer
        {
            IsCpuEnabled = true,
            IsGpuEnabled = true,
            IsMotherboardEnabled = true,
            IsMemoryEnabled = true,
            IsStorageEnabled = true,
            IsControllerEnabled = true,
            IsNetworkEnabled = true,
        };

        bool opened = false;
        try
        {
            _computer.Open();
            opened = true;
        }
        catch (Exception ex)
        {
            EmitError("open_failed", ex);
        }

        List<IHardware> allHardware = new();
        List<HardwareInfo> hardwareInfos = new();

        if (opened)
        {
            try
            {
                foreach (IHardware root in _computer.Hardware)
                {
                    CollectHardware(root, allHardware);
                }

                foreach (IHardware hw in allHardware)
                {
                    var info = new HardwareInfo
                    {
                        Id = hw.Identifier.ToString(),
                        Name = hw.Name,
                        Type = hw.HardwareType.ToString(),
                    };

                    foreach (ISensor sensor in hw.Sensors)
                    {
                        string id = $"{info.Id}/{sensor.SensorType}/{sensor.Index}";
                        info.Sensors.Add(new SensorInfo
                        {
                            Hardware = hw,
                            Sensor = sensor,
                            Id = id,
                            Name = sensor.Name,
                            Type = sensor.SensorType.ToString(),
                            Unit = UnitFor(sensor.SensorType),
                        });
                    }

                    hardwareInfos.Add(info);
                }
            }
            catch (Exception ex)
            {
                EmitError("enumeration_failed", ex);
            }
        }

        EmitHeader(hardwareInfos);

        while (true)
        {
            long iterationStart = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds();

            foreach (IHardware hw in allHardware)
            {
                try
                {
                    hw.Update();
                }
                catch
                {
                    // Ignore a single hardware update failure.
                }
            }

            var values = new Dictionary<string, float>();
            foreach (HardwareInfo hw in hardwareInfos)
            {
                foreach (SensorInfo sensor in hw.Sensors)
                {
                    try
                    {
                        float? v = sensor.Sensor.Value;
                        if (v.HasValue && float.IsFinite(v.Value))
                        {
                            values[sensor.Id] = v.Value;
                        }
                    }
                    catch
                    {
                        // Ignore a single sensor read failure.
                    }
                }
            }

            EmitValues(iterationStart, values);

            long elapsed = DateTimeOffset.UtcNow.ToUnixTimeMilliseconds() - iterationStart;
            long wait = 1000 - elapsed;
            if (wait > 0)
            {
                Thread.Sleep((int)wait);
            }
        }
    }

    private static void CollectHardware(IHardware hardware, List<IHardware> into)
    {
        into.Add(hardware);
        foreach (IHardware sub in hardware.SubHardware)
        {
            CollectHardware(sub, into);
        }
    }

    private static void EmitHeader(List<HardwareInfo> hardwareInfos)
    {
        string json = JsonString(writer =>
        {
            writer.WriteStartObject();
            writer.WriteString("type", "header");
            writer.WriteStartArray("hardware");
            foreach (HardwareInfo hw in hardwareInfos)
            {
                writer.WriteStartObject();
                writer.WriteString("id", hw.Id);
                writer.WriteString("name", hw.Name);
                writer.WriteString("hardwareType", hw.Type);
                writer.WriteStartArray("sensors");
                foreach (SensorInfo sensor in hw.Sensors)
                {
                    writer.WriteStartObject();
                    writer.WriteString("id", sensor.Id);
                    writer.WriteString("name", sensor.Name);
                    writer.WriteString("type", sensor.Type);
                    writer.WriteString("unit", sensor.Unit);
                    writer.WriteEndObject();
                }
                writer.WriteEndArray();
                writer.WriteEndObject();
            }
            writer.WriteEndArray();
            writer.WriteEndObject();
        });

        Console.WriteLine(json);
        Console.Out.Flush();
    }

    private static void EmitValues(long ts, Dictionary<string, float> values)
    {
        string json = JsonString(writer =>
        {
            writer.WriteStartObject();
            writer.WriteString("type", "values");
            writer.WriteNumber("ts", ts);
            writer.WriteStartObject("data");
            foreach (KeyValuePair<string, float> kvp in values)
            {
                writer.WriteNumber(kvp.Key, kvp.Value);
            }
            writer.WriteEndObject();
            writer.WriteEndObject();
        });

        Console.WriteLine(json);
        Console.Out.Flush();
    }

    private static void EmitError(string code, Exception? ex)
    {
        string json = JsonString(writer =>
        {
            writer.WriteStartObject();
            writer.WriteString("type", "error");
            writer.WriteString("code", code);
            writer.WriteString("message", ex?.Message ?? "unknown error");
            writer.WriteEndObject();
        });

        try
        {
            Console.WriteLine(json);
            Console.Out.Flush();
        }
        catch
        {
            // stdout may be gone; nothing else we can do.
        }
    }

    private static string JsonString(Action<Utf8JsonWriter> write)
    {
        using var ms = new MemoryStream();
        using (var writer = new Utf8JsonWriter(ms))
        {
            write(writer);
        }
        return Encoding.UTF8.GetString(ms.ToArray());
    }

    private static string UnitFor(SensorType type) => type switch
    {
        SensorType.Temperature => "°C",
        SensorType.Load => "%",
        SensorType.Fan => "RPM",
        SensorType.Clock => "MHz",
        SensorType.Frequency => "Hz",
        SensorType.Voltage => "V",
        SensorType.Power => "W",
        SensorType.Current => "A",
        SensorType.Flow => "L/h",
        SensorType.Control => "%",
        SensorType.Level => "%",
        SensorType.Factor => "",
        SensorType.Data => "GB",
        SensorType.SmallData => "MB",
        SensorType.Throughput => "B/s",
        SensorType.TimeSpan => "s",
        SensorType.Energy => "mWh",
        SensorType.Noise => "dBA",
        SensorType.Humidity => "%",
        SensorType.Conductivity => "µS/cm",
        _ => "",
    };
}
