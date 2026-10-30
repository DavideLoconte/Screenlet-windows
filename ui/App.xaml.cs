using Microsoft.UI.Xaml;

namespace Screenlet;

public partial class App : Application
{
    Window? window;

    public App()
    {
        var log = System.IO.Path.Combine(System.Environment.GetFolderPath(System.Environment.SpecialFolder.ApplicationData), "Screenlet", "window.log");
        System.IO.Directory.CreateDirectory(System.IO.Path.GetDirectoryName(log)!);
        UnhandledException += (_, e) => System.IO.File.AppendAllText(log, $"{System.DateTime.Now}: {e.Exception}\n");
        InitializeComponent();
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        window = new MainWindow();
        window.Activate();
    }
}
