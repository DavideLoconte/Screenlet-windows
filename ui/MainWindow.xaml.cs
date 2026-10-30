using Microsoft.UI;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;
using System;
using System.IO;
using System.Text.Json;
using System.Text.Json.Nodes;
using System.Threading;
using Windows.Graphics;

namespace Screenlet;

/// The window of the engine that started it: its state as JSON lines on stdin,
/// what the user does as JSON lines on stdout. Ends with the engine.
public sealed partial class MainWindow : Window
{
    readonly StreamWriter engine = new(Console.OpenStandardOutput()) { AutoFlush = true };
    bool loading;
    string devices = "";

    public MainWindow()
    {
        InitializeComponent();
        Title = "Screenlet";
        SystemBackdrop = new MicaBackdrop();
        ExtendsContentIntoTitleBar = true;
        SetTitleBar(TitleArea);
        var base_ = AppContext.BaseDirectory;
        AppWindow.SetIcon(Path.Combine(base_, "Assets", "screenlet.ico"));
        Logo.Source = new BitmapImage(new Uri(Path.Combine(base_, "Assets", "icon.png")));
        var scale = Microsoft.UI.Windowing.DisplayArea.Primary.WorkArea.Height > 900 ? 1.25 : 1.0;
        AppWindow.Resize(new SizeInt32((int)(560 * scale), (int)(700 * scale)));
        Help(PriorityHelp, "Lowest latency sends light frames that arrive sooner, with text a little softer when much changes. Sharpest allows more bandwidth and larger frames, for a more faithful picture that takes longer to arrive.");
        Help(ConnectionHelp, "Automatic uses the cable when the tablet is on it, Wi-Fi otherwise. The cable gives the lowest and steadiest latency, and charges the tablet.");
        Help(ResolutionHelp, "Fewer pixels mean less work for the encoder and less data on the network: less latency, softer text. Automatic uses the native resolution over the cable and, over Wi-Fi, one that suits the tablet, at most 1920 pixels wide on the densest screens.");
        var reader = new Thread(Read) { IsBackground = true };
        reader.Start();
    }

    static void Help(Button button, string text) =>
        button.Flyout = new Flyout { Content = new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap, MaxWidth = 320 } };

    void Read()
    {
        using var input = new StreamReader(Console.OpenStandardInput());
        while (input.ReadLine() is { } line)
            DispatcherQueue.TryEnqueue(() => Apply(line));
        DispatcherQueue.TryEnqueue(Close);
    }

    void Send(JsonObject message) => engine.WriteLine(message.ToJsonString());

    static void Select(ComboBox box, string tag)
    {
        foreach (var item in box.Items)
            if (item is ComboBoxItem { Tag: string t } && t == tag) box.SelectedItem = item;
        if (box.SelectedItem == null && box.Items.Count > 0) box.SelectedIndex = 0;
    }

    /// A list that comes from the engine: redone only when it changed, or a choice being made would close.
    static void Fill(ComboBox box, JsonElement rows)
    {
        if (rows.GetRawText() == box.DataContext as string) return;
        box.DataContext = rows.GetRawText();
        box.Items.Clear();
        foreach (var row in rows.EnumerateArray())
            box.Items.Add(new ComboBoxItem { Content = row.GetProperty("name").GetString(), Tag = row.GetProperty("tag").GetString() });
    }

    void Apply(string line)
    {
        using var message = JsonDocument.Parse(line);
        var root = message.RootElement;
        State.Text = root.GetProperty("state").GetString();
        Dot.Fill = root.GetProperty("tint").GetString() switch
        {
            "on" => new SolidColorBrush(Colors.LimeGreen),
            "ready" => (Brush)Application.Current.Resources["AccentFillColorDefaultBrush"],
            _ => (Brush)Application.Current.Resources["TextFillColorTertiaryBrush"],
        };
        var actions = root.GetProperty("actions");
        if (actions.GetRawText() != devices)
        {
            devices = actions.GetRawText();
            Rebuild(actions);
        }
        var settings = root.GetProperty("settings");
        loading = true;
        var extendable = settings.GetProperty("extendable").GetBoolean();
        Display.Visibility = extendable ? Visibility.Visible : Visibility.Collapsed;
        Install.Visibility = extendable ? Visibility.Collapsed : Visibility.Visible;
        Install.IsEnabled = !settings.GetProperty("installing").GetBoolean();
        var extending = extendable && settings.GetProperty("display").GetString() == "extend";
        MonitorCard.Visibility = extending ? Visibility.Collapsed : Visibility.Visible;
        ResolutionCard.Visibility = extending ? Visibility.Visible : Visibility.Collapsed;
        Select(Display, extending ? "extend" : "mirror");
        Fill(Monitor, settings.GetProperty("monitors"));
        Select(Monitor, settings.GetProperty("mirror").GetString()!);
        Fill(Resolution, settings.GetProperty("resolutions"));
        Select(Resolution, settings.GetProperty("resolution").GetString()!);
        Select(Connection, settings.GetProperty("connection").GetString()!);
        Select(Priority, settings.GetProperty("priority").GetString()!);
        Select(Fps, settings.GetProperty("fps").ToString());
        Select(Codec, settings.GetProperty("codec").GetString()!);
        Select(Kbps, settings.GetProperty("kbps").ToString());
        Select(Vbv, ((int)settings.GetProperty("vbv_frames").GetDouble()).ToString());
        Select(Packets, settings.GetProperty("packets").GetString()!);
        Select(Keyframes, settings.GetProperty("keyframes").ToString());
        Login.IsOn = settings.GetProperty("login").GetBoolean();
        AutoStart.IsOn = settings.GetProperty("auto_start").GetBoolean();
        loading = false;
        Stats.Text = root.GetProperty("stats").GetString();
    }

    void Rebuild(JsonElement actions)
    {
        Devices.Children.Clear();
        if (actions.GetArrayLength() == 0)
        {
            var empty = new Border { Style = (Style)Application.Current.Resources["Card"] };
            empty.Child = new TextBlock
            {
                Text = "Open Screenlet on the tablet, on the same Wi-Fi as this PC",
                Foreground = (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"],
                TextWrapping = TextWrapping.Wrap,
            };
            Devices.Children.Add(empty);
        }
        // One card for each device, with a button for each thing to do with it.
        string? last = null;
        var buttons = new StackPanel();
        foreach (var action in actions.EnumerateArray())
        {
            var device = action.GetProperty("device").GetString() + "\n" + action.GetProperty("detail").GetString();
            if (device != last)
            {
                last = device;
                var card = new Border { Style = (Style)Application.Current.Resources["Card"] };
                var row = new Grid { ColumnSpacing = 12 };
                row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                row.ColumnDefinitions.Add(new ColumnDefinition());
                row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                row.Children.Add(new FontIcon { Glyph = "\uE70A", FontSize = 20 });
                var text = new StackPanel { VerticalAlignment = VerticalAlignment.Center };
                text.Children.Add(new TextBlock { Text = action.GetProperty("device").GetString(), Style = (Style)Application.Current.Resources["BodyStrongTextBlockStyle"] });
                text.Children.Add(new TextBlock
                {
                    Text = action.GetProperty("detail").GetString(),
                    Foreground = (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"],
                    Style = (Style)Application.Current.Resources["CaptionTextBlockStyle"],
                    TextWrapping = TextWrapping.Wrap,
                });
                Grid.SetColumn(text, 1);
                row.Children.Add(text);
                buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, VerticalAlignment = VerticalAlignment.Center };
                Grid.SetColumn(buttons, 2);
                row.Children.Add(buttons);
                card.Child = row;
                Devices.Children.Add(card);
            }
            var id = action.GetProperty("id").GetString();
            if (action.GetProperty("short").GetString() is { Length: > 0 } label)
            {
                var button = new Button { Content = label };
                if (action.GetProperty("primary").GetBoolean())
                    button.Style = (Style)Application.Current.Resources["AccentButtonStyle"];
                button.Click += (_, _) => Send(new JsonObject { ["run"] = id });
                buttons.Children.Add(button);
            }
        }
    }

    void InstallClicked(object sender, RoutedEventArgs e) => Send(new JsonObject { ["run"] = "install" });

    void DefaultsClicked(object sender, RoutedEventArgs e) => Send(new JsonObject { ["run"] = "defaults" });

    void Changed(object sender, SelectionChangedEventArgs e)
    {
        if (!loading && sender is ComboBox { Tag: string key, SelectedItem: ComboBoxItem { Tag: string value } })
            Send(new JsonObject { ["set"] = key, ["value"] = value });
    }

    void Toggled(object sender, RoutedEventArgs e)
    {
        if (!loading && sender is ToggleSwitch { Tag: string key } toggle)
            Send(new JsonObject { ["set"] = key, ["value"] = toggle.IsOn ? "1" : "0" });
    }
}
